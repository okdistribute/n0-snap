//! Bounded CARv1 / signed repository inclusion verification. No JSON getRecord
//! response is treated as proof of account ownership.
use anyhow::{Context, Result, bail, ensure};
use ipld_core::{cid::Cid, ipld::Ipld};
use p256::ecdsa::signature::Verifier;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, io::Cursor};

pub fn encode(value: &Ipld) -> Result<Vec<u8>> {
    Ok(serde_ipld_dagcbor::to_vec(value)?)
}
pub fn map(value: &Ipld) -> Result<&std::collections::BTreeMap<String, Ipld>> {
    if let Ipld::Map(m) = value {
        Ok(m)
    } else {
        bail!("Expected CBOR map")
    }
}
pub fn string(value: &Ipld) -> Result<&str> {
    if let Ipld::String(s) = value {
        Ok(s)
    } else {
        bail!("Expected string")
    }
}
fn link(value: &Ipld) -> Result<Cid> {
    if let Ipld::Link(cid) = value {
        Ok(*cid)
    } else {
        bail!("Expected CID")
    }
}
fn bytes(value: &Ipld) -> Result<&[u8]> {
    if let Ipld::Bytes(b) = value {
        Ok(b)
    } else {
        bail!("Expected bytes")
    }
}
fn field<'a>(value: &'a Ipld, key: &str) -> Result<&'a Ipld> {
    map(value)?
        .get(key)
        .with_context(|| format!("Missing {key}"))
}
fn optional_link(value: &Ipld) -> Result<Option<Cid>> {
    if value == &Ipld::Null {
        Ok(None)
    } else {
        link(value).map(Some)
    }
}
fn varint(input: &mut &[u8]) -> Result<usize> {
    let mut value = 0usize;
    for shift in (0..35).step_by(7) {
        let (&byte, rest) = input.split_first().context("Truncated CAR")?;
        *input = rest;
        value |= usize::from(byte & 127) << shift;
        if byte < 128 {
            return Ok(value);
        }
    }
    bail!("Oversized CAR length")
}
fn frame<'a>(input: &mut &'a [u8]) -> Result<&'a [u8]> {
    let len = varint(input)?;
    ensure!(len > 0 && len <= input.len(), "Truncated CAR frame");
    let (frame, rest) = input.split_at(len);
    *input = rest;
    Ok(frame)
}
fn block(blocks: &HashMap<Cid, Vec<u8>>, cid: Cid) -> Result<Ipld> {
    let data = blocks.get(&cid).context("Incomplete repository proof")?;
    let value: Ipld = serde_ipld_dagcbor::from_slice(data)?;
    ensure!(encode(&value)? == *data, "Noncanonical repository block");
    Ok(value)
}
fn depth(key: &[u8]) -> u32 {
    let mut zeros = 0;
    for b in Sha256::digest(key) {
        zeros += b.leading_zeros();
        if b != 0 {
            break;
        }
    }
    zeros / 2
}

/// Verify the current commit's signature, all supplied block hashes, and the
/// exact MST path. `head` comes from a fresh HTTPS getLatestCommit on the DID's
/// authoritative PDS; it prevents accepting a detached old getRecord proof.
pub fn verify(car: &[u8], did: &str, public_key: &str, head: &str, path: &str) -> Result<Ipld> {
    ensure!(car.len() <= 1024 * 1024, "Repository proof exceeds 1 MB");
    let mut input = car;
    let header: Ipld = serde_ipld_dagcbor::from_slice(frame(&mut input)?)?;
    ensure!(
        field(&header, "version")? == &Ipld::Integer(1),
        "Unsupported CAR"
    );
    let Ipld::List(roots) = field(&header, "roots")? else {
        bail!("Missing CAR roots")
    };
    let root = link(roots.first().context("Empty CAR roots")?)?;
    ensure!(
        root == head.parse::<Cid>()?,
        "Repository changed; please retry"
    );
    let mut blocks = HashMap::new();
    let mut count = 0;
    while !input.is_empty() {
        count += 1;
        ensure!(count <= 1024, "Too many proof blocks");
        let data = frame(&mut input)?;
        let mut cursor = Cursor::new(data);
        let cid = Cid::read_bytes(&mut cursor)?;
        let payload = &data[cursor.position() as usize..];
        ensure!(
            cid.version() == ipld_core::cid::Version::V1
                && cid.codec() == 0x71
                && cid.hash().code() == 0x12
                && cid.hash().digest() == Sha256::digest(payload).as_slice(),
            "Invalid repository block hash"
        );
        blocks.insert(cid, payload.to_vec());
    }
    let commit = block(&blocks, root)?;
    ensure!(
        string(field(&commit, "did")?)? == did,
        "Repository issuer mismatch"
    );
    ensure!(
        field(&commit, "version")? == &Ipld::Integer(3),
        "Unsupported repository version"
    );
    optional_link(field(&commit, "prev")?)?;
    let rev = string(field(&commit, "rev")?)?;
    let alphabet = b"234567abcdefghijklmnopqrstuvwxyz";
    ensure!(rev.len() == 13, "Invalid repository revision");
    let mut clock = 0u64;
    for (i, b) in rev.bytes().enumerate() {
        let digit = alphabet
            .iter()
            .position(|x| *x == b)
            .context("Invalid revision")? as u64;
        ensure!(i != 0 || digit < 16, "Invalid revision overflow");
        clock = (clock << 5) | digit;
    }
    ensure!(
        (clock >> 10) / 1_000_000 <= crate::model::now() + 300,
        "Future repository revision"
    );
    let sig = bytes(field(&commit, "sig")?)?;
    let mut unsigned = map(&commit)?.clone();
    unsigned.remove("sig");
    let message = encode(&Ipld::Map(unsigned))?;
    let key = bs58::decode(
        public_key
            .strip_prefix('z')
            .context("Unsupported signing key")?,
    )
    .into_vec()?;
    match key.get(..2) {
        Some([0x80, 0x24]) => {
            let signature = p256::ecdsa::Signature::from_slice(sig)?;
            ensure!(signature.normalize_s().is_none(), "Noncanonical signature");
            p256::ecdsa::VerifyingKey::from_sec1_bytes(&key[2..])?.verify(&message, &signature)?;
        }
        Some([0xe7, 0x01]) => {
            let signature = k256::ecdsa::Signature::from_slice(sig)?;
            ensure!(signature.normalize_s().is_none(), "Noncanonical signature");
            k256::ecdsa::VerifyingKey::from_sec1_bytes(&key[2..])?.verify(&message, &signature)?;
        }
        _ => bail!("Unsupported repository signing curve"),
    }
    let target = path.as_bytes();
    let mut next = link(field(&commit, "data")?)?;
    let mut expected_depth: Option<u32> = None;
    let mut lower = Vec::new();
    let mut upper: Option<Vec<u8>> = None;
    for _ in 0..128 {
        let node = block(&blocks, next)?;
        let Ipld::List(entries) = field(&node, "e")? else {
            bail!("Invalid MST entries")
        };
        ensure!(entries.len() <= 1024, "Oversized MST node");
        let mut branch = optional_link(field(&node, "l")?)?;
        let mut previous = Vec::new();
        let mut node_depth = expected_depth;
        let mut found = None;
        let mut branch_lower = lower.clone();
        let mut branch_upper = upper.clone();
        for entry in entries {
            let Ipld::Integer(prefix) = field(entry, "p")? else {
                bail!("Invalid MST prefix")
            };
            let prefix = usize::try_from(*prefix)?;
            ensure!(prefix <= previous.len(), "Invalid MST prefix length");
            let mut key = previous[..prefix].to_vec();
            key.extend_from_slice(bytes(field(entry, "k")?)?);
            ensure!(
                !key.is_empty()
                    && key.len() <= 1024
                    && key > previous
                    && key > lower
                    && upper.as_ref().is_none_or(|u| key < *u),
                "Invalid MST key order"
            );
            let common = previous
                .iter()
                .zip(&key)
                .take_while(|(a, b)| a == b)
                .count();
            ensure!(common == prefix, "Noncanonical MST prefix");
            let d = depth(&key);
            ensure!(
                node_depth.is_none_or(|expected| d == expected),
                "Invalid MST layer"
            );
            node_depth = Some(d);
            let value = link(field(entry, "v")?)?;
            let right = optional_link(field(entry, "t")?)?;
            if key.as_slice() == target {
                found = Some(value);
            }
            if key.as_slice() < target {
                branch = right;
                branch_lower = key.clone();
            } else if key.as_slice() > target && branch_upper.as_ref().is_none_or(|u| key < *u) {
                branch_upper = Some(key.clone());
            }
            previous = key;
        }
        if let Some(value) = found {
            return block(&blocks, value);
        }
        next = branch.context("Device record is not included in this repository")?;
        expected_depth = Some(
            node_depth
                .context("Empty root MST")?
                .checked_sub(1)
                .context("Invalid MST depth")?,
        );
        lower = branch_lower;
        upper = branch_upper;
    }
    bail!("Repository proof is too deep")
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::signature::Signer;
    use std::collections::BTreeMap;
    fn obj(fields: &[(&str, Ipld)]) -> Ipld {
        Ipld::Map(
            fields
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }
    fn cid(bytes: &[u8]) -> Cid {
        Cid::new_v1(
            0x71,
            ipld_core::cid::multihash::Multihash::wrap(0x12, &Sha256::digest(bytes)).unwrap(),
        )
    }
    fn put_frame(out: &mut Vec<u8>, data: &[u8]) {
        let mut n = data.len();
        while n >= 128 {
            out.push((n as u8 & 127) | 128);
            n >>= 7;
        }
        out.push(n as u8);
        out.extend_from_slice(data);
    }
    fn fixture(k256_curve: bool) -> (Vec<u8>, String, String, Ipld) {
        let record = obj(&[("hello", Ipld::String("world".into()))]);
        let record_bytes = encode(&record).unwrap();
        let tree = obj(&[
            ("l", Ipld::Null),
            (
                "e",
                Ipld::List(vec![obj(&[
                    ("p", Ipld::Integer(0)),
                    ("k", Ipld::Bytes(b"test.example.record/self".to_vec())),
                    ("v", Ipld::Link(cid(&record_bytes))),
                    ("t", Ipld::Null),
                ])]),
            ),
        ]);
        let tree_bytes = encode(&tree).unwrap();
        let mut commit = BTreeMap::from([
            ("did".into(), Ipld::String("did:plc:alice".into())),
            ("version".into(), Ipld::Integer(3)),
            ("data".into(), Ipld::Link(cid(&tree_bytes))),
            ("rev".into(), Ipld::String("2222222222222".into())),
            ("prev".into(), Ipld::Null),
        ]);
        let unsigned = encode(&Ipld::Map(commit.clone())).unwrap();
        let (key, sig) = if k256_curve {
            let signer = k256::ecdsa::SigningKey::from_bytes((&[7u8; 32]).into()).unwrap();
            let sig: k256::ecdsa::Signature = signer.sign(&unsigned);
            (
                [
                    vec![0xe7, 0x01],
                    signer
                        .verifying_key()
                        .to_encoded_point(true)
                        .as_bytes()
                        .to_vec(),
                ]
                .concat(),
                sig.normalize_s().unwrap_or(sig).to_bytes().to_vec(),
            )
        } else {
            let signer = p256::ecdsa::SigningKey::from_bytes((&[7u8; 32]).into()).unwrap();
            let sig: p256::ecdsa::Signature = signer.sign(&unsigned);
            (
                [
                    vec![0x80, 0x24],
                    signer
                        .verifying_key()
                        .to_encoded_point(true)
                        .as_bytes()
                        .to_vec(),
                ]
                .concat(),
                sig.normalize_s().unwrap_or(sig).to_bytes().to_vec(),
            )
        };
        commit.insert("sig".into(), Ipld::Bytes(sig));
        let commit_bytes = encode(&Ipld::Map(commit)).unwrap();
        let root = cid(&commit_bytes);
        let header = obj(&[
            ("version", Ipld::Integer(1)),
            ("roots", Ipld::List(vec![Ipld::Link(root)])),
        ]);
        let mut car = Vec::new();
        put_frame(&mut car, &encode(&header).unwrap());
        for bytes in [record_bytes, tree_bytes, commit_bytes] {
            put_frame(&mut car, &[cid(&bytes).to_bytes(), bytes].concat());
        }
        (
            car,
            format!("z{}", bs58::encode(key).into_string()),
            root.to_string(),
            record,
        )
    }
    #[test]
    fn verifies_both_curves_and_exact_repository_path() {
        for curve in [false, true] {
            let (car, key, root, record) = fixture(curve);
            assert_eq!(
                verify(
                    &car,
                    "did:plc:alice",
                    &key,
                    &root,
                    "test.example.record/self"
                )
                .unwrap(),
                record
            );
            assert!(
                verify(
                    &car,
                    "did:plc:mallory",
                    &key,
                    &root,
                    "test.example.record/self"
                )
                .is_err()
            );
            assert!(
                verify(
                    &car,
                    "did:plc:alice",
                    &key,
                    &root,
                    "test.example.record/deleted"
                )
                .is_err()
            );
            assert!(
                verify(
                    &car,
                    "did:plc:alice",
                    &key,
                    &cid(b"different current head").to_string(),
                    "test.example.record/self"
                )
                .is_err()
            );
            let mut tampered = car.clone();
            *tampered.last_mut().unwrap() ^= 1;
            assert!(
                verify(
                    &tampered,
                    "did:plc:alice",
                    &key,
                    &root,
                    "test.example.record/self"
                )
                .is_err()
            );
            let (_, wrong_key, _, _) = fixture(!curve);
            assert!(
                verify(
                    &car,
                    "did:plc:alice",
                    &wrong_key,
                    &root,
                    "test.example.record/self"
                )
                .is_err()
            );
            assert!(
                verify(
                    &car[..car.len() / 2],
                    "did:plc:alice",
                    &key,
                    &root,
                    "test.example.record/self"
                )
                .is_err()
            );
        }
    }
    #[test]
    fn rejects_oversized_or_malformed_archives() {
        for car in [vec![0; 1024 * 1024 + 1], vec![255; 32], vec![], vec![1, 0]] {
            assert!(verify(&car, "did:plc:alice", "z", "bad", "test/self").is_err());
        }
    }
}
