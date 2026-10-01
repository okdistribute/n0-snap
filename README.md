# n0-snap

A native Rust/Dioxus app for macOS and iPhone, for sharing disappearing photos
and videos with **iroh 1.3.0**. AT Protocol finds people; native iroh carries media.
The interface uses the system web-view, but networking, encryption, and storage
run in Rust on the device. There is no browser/WASM client or hosted demo.

## Open it

On macOS, clone the repository and build the app:

```sh
git clone https://github.com/okdistribute/n0-snap.git
cd n0-snap
sh scripts/build-app.sh
open n0-snap.app
```

Or run directly during development:

```sh
cargo run --bin flicker
```

The generated app bundle is not checked into Git. Rust and the usual Dioxus
desktop platform dependencies are required. The
checked-in lockfile pins the dependency versions. Styles
are embedded, so opening the interface does not require a web server. The app
uses system fonts.

The prototype’s Cargo binaries, invitation scheme, and data-directory settings
retain their original `flicker` names for compatibility with existing installs.

## iPhone demo

Open `ios/n0-snap.xcodeproj` in Xcode. Select the **n0-snap** target, choose your
Apple development team under **Signing & Capabilities**, and select your
connected, unlocked iPhone as the run destination. Enable Developer Mode on
the phone if Xcode asks. Run the project to build and install the native app.
The project invokes Cargo and uses automatic signing; it does not contain
any developer credentials or a hard-coded team ID. iOS 16 or newer is required.

1. Sign in to Bluesky on the phone.
2. On the other device, open **My Snapcode**.
3. On iPhone, choose **Add Snapcode → Scan QR code**, allow camera access,
   and point at that code. Review and choose **Request connection**.
4. Accept under **Friends** on the other device. Keep both apps in the foreground.
5. Use **New snap** to select a photo or video from the native iOS photo picker.

The scanner uses AVFoundation; no frames leave the phone. The picker grants
access only to media you select, not your entire library. HEIC photos are
converted to JPEG. The same 12 MiB media limit applies on phone and desktop.
Invitations opened through the registered `flicker://friend/…` URL scheme are
held through sign-in and still require a connection confirmation. Without
the app installed, a QR code cannot install or run the demo by itself.

To build without provisioning a physical device:

```sh
xcodebuild -project ios/n0-snap.xcodeproj -scheme n0-snap \
  -sdk iphonesimulator -destination 'generic/platform=iOS Simulator' \
  -derivedDataPath target/ios-xcode CODE_SIGNING_ALLOWED=NO build
```

Simulator builds can verify launch and layout; camera scanning and real-device
network behavior require a physical iPhone. This demo has no background delivery
or push notifications: iOS can suspend connections when you leave the app.

## Try it

1. Enter your Bluesky handle and choose **Continue with Bluesky**. Approve the
   sign-in in your browser; the app opens after your identity is verified.
2. Under **Friends**, choose **Make this device discoverable** to publish a
   public device record in your PDS. Do this on both devices. Existing sessions
   from older builds need one sign-out/sign-in to grant the new permission.
3. **Discover** loads the people you follow. Choose **Connect via Bluesky** on
   a profile, select a verified device, and request a connection. The recipient
   accepts in **Friends**. Both native apps must be open. A follow alone grants
   no access. Exchanging a **Snapcode** remains an optional fallback.
4. Choose **New snap**, select a local photo/video, select a connected friend,
   and send. Switch to **24-hour story** to share with all connected friends.
5. Open the received media in **Snaps**. The 60-second timer starts on opening.
   Choose **Save to Memories** to keep a local copy. Closing a direct Snap
   ends its viewing window early; a saved copy remains available.
6. Visit **Hosting** to test and select a local, personal, or cloud host.

Snaps, Stories, and Memories use media galleries. There is no chat or inbox.

Fresh accounts start empty, with no sample friends or simulated sends. Discover
uses authenticated Bluesky requests through your account's PDS. Search and
Following support pagination. Follow connections
sample up to 6 accounts from the first 100 follows and up to 100 follows per
sampled account; cards name the actual accounts that follow each suggestion.
This is a partial public graph, not an authenticated mutual-friend list.

Browsing a handle does not switch your signed-in identity. Profiles aren't
filtered to n0-snap users: device records are fetched and verified only when
you choose **Connect via Bluesky**, not for every profile on the page.

## PDS device registry (experimental)

There is no separate central registry. Each account stores its devices at
`at://<account DID>/io.github.okdistribute.n0snap.device/<64-character endpoint ID>`.
The checked-in Lexicon is in `lexicons/`. This is an application-specific
experimental namespace, not a ratified atproto/iroh standard or a deployed
Lexicon authority. Publication uses `validate: false`; clients validate strictly.
It follows the [atproto + iroh draft](https://mfzx.net/drafts/iroh-with-atp).

Records contain `$type`, `iss` (account DID), `sub` (Ed25519 `did:key`), `via`
(the versioned friend-request ALPN), and `proof` (device signature over canonical
DRISL CBOR without `proof`). No IP addresses, Snapcode capabilities, private
keys, OAuth tokens, or media appear in the public record. Publication is opt-in;
sign-in alone never creates a record. Each installation has its own key/record.

Discovery resolves the DID's authoritative PDS, fetches at most 16 records,
and verifies CAR block hashes, the repository commit signature (P-256 or
secp256k1), exact MST inclusion, the issuer, and the device's Ed25519 signature.
It checks the commit against a fresh HTTPS `getLatestCommit`. A connection's
authenticated iroh remote key must match that record. Names are fetched by the
verified account DID, not accepted from the peer. A registered device is only
a candidate: the recipient must still explicitly accept. Native iroh resolves
endpoint routes; PDS records are identity bindings, not a routing registry.

**Remove device record** deletes this installation's record. Account-based
requests and new media connections recheck both records without a validity
cache and fail closed on missing records or lookup/verification failures.
Revocation does not erase already received media or stop an already authorized
transfer. Separately approved Snapcode-only contacts do not depend on PDS records.
Signing out closes endpoints but does not delete the public record.

Current limits: public HTTPS PDS origins on port 443, `did:plc` and domain-root
`did:web`, up to 16 devices per account, current signing keys only. Directory
responses are bounded and public DNS addresses are pinned per lookup; private
and local-network destinations are rejected. A busy repository can change
between proof and head reads; retry in that case. Freshness trusts the DID's
current HTTPS PDS to report its latest head; a signature by itself cannot prove
that a previously valid record has not subsequently been deleted.

## Bluesky sign-in

The native OAuth flow uses ATrium with PKCE, DPoP, pushed authorization requests,
state/issuer validation, and a loopback callback at `127.0.0.1:47839`.
On iPhone, Safari's system view presents sign-in while the native callback
listener stays in the foreground. The view closes when sign-in completes.
This development flow retains localhost registration; a distributed release
should use registered native client metadata and ASWebAuthenticationSession.
iPhone handle DNS lookup uses Google's resolvers (the same fallback as iroh)
because iOS does not expose `/etc/resolv.conf`; HTTPS resolution is the fallback.
Passwords are entered only in Bluesky's browser page. The requested permissions
cover identity, profile lookup/search, public follows, and writes only to
`io.github.okdistribute.n0snap.device`; they do not allow posting or reading
Bluesky chats. AppView requests use a separate proxy-configured session clone;
repository writes go directly to the signed-in account's PDS.

This build uses AT Protocol's localhost client registration, so Bluesky may
label it as a development client. Branded distribution will need published
client metadata. Sign in to local test instances one at a time because they
share the callback port. The listener closes after sign-in, cancellation, or
a five-minute timeout.

OAuth credentials persist in `auth/session.json` (0600, inside a 0700 directory
on Unix). Expired access tokens refresh during authenticated requests. Restoring
a session verifies it with an authenticated profile request before showing the
app. **Sign out** closes the media endpoints, attempts server revocation, and
clears local OAuth credentials even if the server is offline.

Each DID has its own local data and device identity under `accounts/<DID hash>`.
Previous prototype data in the root directory remains untouched and is not
automatically assigned to an account. Signing out preserves account data.

## Connect two real installations

Choose **My Snapcode** in the top bar to show your QR invitation. Under Friends,
you can set your device's name. The QR contains a native device invitation.
Scan it from the iPhone app or send the
`flicker://friend/…` invitation to the other person. They paste it into
**Add Snapcode → Request connection**. You review the request under **Friends**
and choose **Accept** or **Decline**. After acceptance, their app checks status
and connects automatically (normally within 8–20 seconds while both are online).
Only one person needs to share their code. iPhone includes a native camera scanner;
desktop accepts decoded invitations as text.
Codes from the earlier prototype still use the old bilateral-add flow.

Requests use a separate iroh protocol, a secret capability in the shared code,
and the authenticated remote device key. A pending request never grants media
access. Duplicate retries are coalesced; requests and declines survive restart.
Outgoing requests retry while the app is open and can be canceled under Friends.
Cancel stops local retries; it doesn't retract an already delivered request.
The prototype permits 32 pending outgoing requests and 128 distinct incoming
request senders per process. Share Snapcodes through a channel you trust.

Both apps must be open to deliver requests or new Snap/Story invitations. Each
side only accepts media from explicitly approved endpoint keys. The
media is uploaded to the sender's selected storage host and downloaded by the
recipient using an individual read capability. Once the invitation has been
received, a cloud or personal host lets the recipient fetch the media even
after the sender closes their app.

To test with separate local data, run a second process from a terminal:

```sh
FLICKER_DATA_DIR="$PWD/.data/second-person" cargo run --bin flicker
```

## Run your own media host

```sh
export FLICKER_STORE_TOKEN="$(openssl rand -hex 32)"
cargo run --no-default-features --bin flicker-store -- .data/my-host
```

Copy the printed endpoint ID into **Hosting → My own server** and enter
the same write token. **Test & save** authenticates a real request over iroh
before changing the app's host. Keep the token private: it authorizes uploads
and is distinct from the per-object read capability sent to a friend.

The exact same executable runs on a cloud VM. Keep its data directory on a
persistent disk so the endpoint identity and encrypted objects survive a
restart. Choose **Cloud endpoint** in the app and enter its ID/token. This
prototype does not deploy or provision a hosted service automatically.

This media host can run alongside a user's AT Protocol PDS on the same server.
It is a separate iroh service, not an extension of the AT Protocol repository
API. Snaps are never published as public AT records or PDS blobs.

Objects expire within 24 hours and are rejected immediately after their
deadline. A cleanup job removes expired files every 30 seconds. The prototype
has a 12 MiB media limit, a 512 MiB per-host quota, and bounded concurrent
requests. It uses iroh's default address lookup and relays. For a deployed
service, configure suitable relay infrastructure and operational limits.

## What is implemented

- Dioxus Snap, Story, and Memory galleries, compose, Discover, QR invite, and hosting.
- Browser OAuth sign-in, persisted sessions, refresh, sign-out, and account isolation.
- Native iPhone build, AVFoundation QR scanner, Photos picker, invitation deep links,
  and safe-area-aware mobile layout.
- Public name/handle search, paginated follows, sampled connection suggestions,
  direct Snapcode connection actions, loading/error/empty states, and stale-search protection.
- Iroh connection requests, explicit Accept/Decline, and automatic connection
  after acceptance; public discovery alone never authorizes messaging.
- AES-256-GCM encryption before upload; hosts store ciphertext and never
  receive decryption keys.
- Iroh 1.3 storage upload/download and direct peer invitation delivery.
- Device-key allowlisting for inbound peer requests.
- Persisted viewing deadlines; expired items lose their local ticket/key
  unless explicitly saved. Memories retain encrypted media locally and can
  be viewed without their remote host.
- Separate runnable storage endpoint, stable endpoint keys, write-token
  authentication, per-object read capabilities, expiry, and quotas.

## Prototype boundaries

Signed DID/device attestations, automatic n0-snap membership
discovery, offline
invitation mailboxes, push notifications, receipt synchronization, direct photo/video
camera capture, and App Store distribution are not implemented. Public profile discovery
does not verify that a peer controls an AT Protocol account. Snapcodes verify
an iroh device key only. Compare codes through a channel you trust.

Secrets and Memories are stored in owner-readable local files (0600 on Unix).
Media is encrypted, but the local state also contains its decryption keys;
OS keychain integration and protection against a compromised local account
are still needed. This is an experiment, not a reviewed secure messenger.

The viewer runs for 60 seconds, including for longer imported video files;
videos are not transcoded or trimmed. Recipients can save or capture content.
Saving to Memories currently stays local and does not notify the sender.
Story access uses bearer capabilities; removing a friend does not revoke
an already-shared capability. Direct connections expose peers' IP addresses.

Local app state defaults to the platform application-data directory under
`flicker-prototype`. Set `FLICKER_DATA_DIR` to use a different directory.

## Verify

```sh
cargo check --all-targets
cargo test --lib
```

The network tests run real iroh endpoints and check encrypted round-trip,
invalid read capabilities, invalid storage tokens, tampered keys, friend request
capabilities, endpoint impersonation, deduplication, declines, and rejection of
media before acceptance. Unit tests cover follow ranking and state migration.
OAuth tests exercise a complete mock-provider callback, wrong state/issuer
rejection, PKCE, authenticated profile retrieval, session persistence, refresh,
revocation, and account isolation. To check that live Bluesky accepts the
authorization request without signing in:

```sh
cargo test --lib auth::tests::live_bluesky_accepts_authorization_request -- --ignored
```

## Sources

- [iroh-with-atp design inspiration](https://mfzx.net/drafts/iroh-with-atp)
- [iroh protocols](https://docs.iroh.computer/protocols/writing-a-protocol)
- [Dioxus desktop](https://dioxuslabs.com/learn/0.7/guides/platforms/desktop/)
- [AT Protocol identity](https://atproto.com/guides/identity)
- [AT Protocol OAuth](https://atproto.com/specs/oauth)
- [Official profile search schema](https://github.com/bluesky-social/atproto/blob/main/lexicons/app/bsky/actor/searchActors.json)
- [Official follows schema](https://github.com/bluesky-social/atproto/blob/main/lexicons/app/bsky/graph/getFollows.json)
- Sample photographs: [lake](https://images.unsplash.com/photo-1476514525535-07fb3b4ae5f1),
  [mountains](https://images.unsplash.com/photo-1464822759023-fed622ff2c3b),
  [flowers](https://images.unsplash.com/photo-1490750967868-88aa4486c946), via Unsplash.
