# n0-snap

A runnable Dioxus desktop experiment in disappearing messages, powered by
**iroh 1.3.0**. AT Protocol finds people; iroh carries media and peer messages.

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
checked-in lockfile pins the dependency versions. Sample photos and styles
are embedded, so opening the interface does not require a web server. Fonts
use Google Fonts when reachable, with system font fallbacks.

The prototype’s Cargo binaries, invitation scheme, and data-directory settings
retain their original `flicker` names for compatibility with existing installs.

## Try it

1. Open Mira's sample snap. The 60-second timer starts when the media opens.
2. Save it to Memories, close the viewer, and find it under Memories. Closing
   a direct Snap ends its viewing window early; a saved copy remains available.
3. Choose **New snap**, select a sample photo or a local photo/video, and send.
   Even sample sends encrypt, upload, and fetch through two real iroh endpoints.
4. Switch the composer to **24-hour story** to publish a Story.
5. Open **Discover** to search by name or handle, browse a handle’s public follows,
   explore follow connections, and save profiles. Use **Friends** for Snapcodes
   and incoming connection requests.
6. Visit **Your hosting** to test and select a local, personal, or cloud host.

Sample contacts are fictional and explicitly marked. Sending to one adds a
local outgoing preview; it does not message anyone on Bluesky. Discover uses
Bluesky's public API. Search and Following support pagination. Follow connections
sample up to 6 accounts from the first 100 follows and up to 100 follows per
sampled account; cards name the actual accounts that follow each suggestion.
This is a partial public graph, not an authenticated mutual-friend list.

Saved profiles live in Discover, separate from connected conversations. Browsing
a handle does not sign you in or claim it as your identity. Profiles aren't yet
filtered to n0-snap users: verified AT-to-device bindings and account sign-in are
still required for that. A real Snapcode is needed before requesting a connection.

## Connect two real installations

Under Friends, set your name and show your Snapcode. Send the
`flicker://friend/…` invitation to the other person. They paste it into
**Add a friend → Request connection**. You review the request under **Friends**
and choose **Accept** or **Decline**. After acceptance, their app checks status
and connects automatically (normally within 8–20 seconds while both are online).
Only one person needs to share their code. The QR contains the same invitation;
this prototype accepts decoded invitations as text and has no camera QR scanner.
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

Copy the printed endpoint ID into **Your hosting → My own server** and enter
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

- Dioxus inbox, Stories, Memories, compose, Discover, QR invite, and hosting.
- Public name/handle search, paginated follows, sampled connection suggestions,
  saved profiles, loading/error/empty states, and stale-search protection.
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

AT Protocol OAuth, signed DID/device attestations, automatic n0-snap membership
discovery, offline
invitation mailboxes, push notifications, receipt synchronization, camera
capture, and mobile packaging are not implemented. Public profile discovery
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

## Sources

- [iroh-with-atp design inspiration](https://mfzx.net/drafts/iroh-with-atp)
- [iroh protocols](https://docs.iroh.computer/protocols/writing-a-protocol)
- [Dioxus desktop](https://dioxuslabs.com/learn/0.7/guides/platforms/desktop/)
- [AT Protocol identity](https://atproto.com/guides/identity)
- [Official profile search schema](https://github.com/bluesky-social/atproto/blob/main/lexicons/app/bsky/actor/searchActors.json)
- [Official follows schema](https://github.com/bluesky-social/atproto/blob/main/lexicons/app/bsky/graph/getFollows.json)
- Sample photographs: [lake](https://images.unsplash.com/photo-1476514525535-07fb3b4ae5f1),
  [mountains](https://images.unsplash.com/photo-1464822759023-fed622ff2c3b),
  [flowers](https://images.unsplash.com/photo-1490750967868-88aa4486c946), via Unsplash.
