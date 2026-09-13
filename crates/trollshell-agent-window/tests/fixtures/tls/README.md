# TLS fixtures for the launch-time verify (#1234)

The certificates `src/verify.rs`'s and `src/tls.rs`'s **`system-tests`** blocks
run against: a local `GTlsServerConnection` on one side of a loopback socket
and the window's own `verify::probe` on the other, so the whole route-2 path —
open a connection, hand GIO a `GTlsFileDatabase` over the hive's anchors, read
the verdict, pin the leaf — is exercised by the same code a launch runs.

They live here rather than being minted at test time because a check
derivation that mints certificates would need `openssl` in its closure **and**
would make the tests depend on the clock. `generate.sh` in this directory is
the record of how they were made; it runs in the devShell (`openssl` is there,
and deliberately not on the bare PATH).

| file                            | what it is                                                                                                                                 |
| ------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ |
| `fixture-ca.pem`                | a self-signed CA, shaped like the hive CA `hive-tls.nix` mints. **This is the anchors file** a test hands `TROLLSHELL_AGENT_WINDOW_CA`.    |
| `server-leaf.pem` / `-key.pem`  | the leaf that CA issued for `hive.local` — what a gateway presents. Valid for a century.                                                   |
| `other-leaf.pem` / `-key.pem`   | the same subject and SANs under a **different** CA, which `fixture-ca.pem` does not carry. Drives the `UNKNOWN_CA` case.                   |
| `expired-leaf.pem` / `-key.pem` | issued by the **right** CA and expired since 2021-01-01, so the only thing wrong with it is the clock. Drives the `EXPIRED` case.          |
| `gateway.pem`                   | hyperhive's own shape: `cat server-leaf.pem fixture-ca.pem` — **leaf first, CA appended**, which is what lets route 3 pin its first block. |
| `gateway-ca-first.pem`          | the same two certificates the other way round — what a _trust bundle_ looks like, and what the card has to be able to complain about.      |

The SANs are `DNS:hive.local, DNS:*.hive.local, IP:127.0.0.1`. The loopback
entry is load-bearing: a test binds an ephemeral port on `127.0.0.1` and
connects to it by address, so without it every probe would come back
`BAD_IDENTITY` and the tests would be asserting the wrong failure.

**None of this is secret.** The CA keys were deleted by the generator, the
three server keys that remain are 2048-bit throwaways whose only job is to let
a test bind a TLS listener on loopback, nothing anywhere trusts any of these
certificates, and `hive.local` is not a resolvable name.

The three PEMs one directory up (`../hive-ca.pem`, `../gateway-leaf.pem`,
`../trust-bundle.pem`) are #1130's, for the hermetic
`a_bundle_is_not_the_certificate_the_gateway_presents`, and carry **no** keys.
They are left alone: that test runs in the default `cargo test`, which has no
TLS backend at all, and re-pointing it here would only make it harder to see
which assertions need one.
