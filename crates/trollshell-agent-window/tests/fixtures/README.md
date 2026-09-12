# TLS fixtures

Three PEMs, for one test: `src/tls.rs`'s
`a_bundle_is_not_the_certificate_the_gateway_presents`.

They exist because `TROLLSHELL_AGENT_WINDOW_CERT` reaches
`webkit_network_session_allow_tls_certificate_for_host`, which **pins the
certificate the server presents** — it is an exception for one certificate, not
a trust anchor. Pointing it at hyperhive's `trust-bundle.pem` (which starts
with the hive CA) produces a value that can never match what the gateway sends,
and the load fails behind a log line that says it worked. The test asserts the
two are different certificates, so the prose in `src/tls.rs` is written against
something rather than against a plausible-sounding guess.

| file               | what it is                                                                                                                    |
| ------------------ | ----------------------------------------------------------------------------------------------------------------------------- |
| `hive-ca.pem`      | a self-signed CA, shaped like the hive CA `nix/host-modules/hive-tls.nix` mints (`CA:TRUE, pathlen:0`, `keyCertSign,cRLSign`) |
| `gateway-leaf.pem` | a leaf for `hive.local` issued by it — what a gateway actually presents                                                       |
| `trust-bundle.pem` | the anchor bundle a _trust store_ wants: **CA first**, never the leaf                                                         |

**No private keys are here.** They were deleted by the generator; nothing in
this directory can sign anything, and none of these certificates is trusted by
anything anywhere. `hive.local` is not a resolvable name.

Regenerate (the same script that made them, kept here as the record of how):

```sh
openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 36500 \
  -keyout .ca-key.pem -out hive-ca.pem \
  -subj "/CN=hive-ca hive.local (trollshell test fixture)" \
  -addext "basicConstraints=critical,CA:TRUE,pathlen:0" \
  -addext "keyUsage=critical,keyCertSign,cRLSign"

openssl req -newkey rsa:2048 -nodes -keyout .leaf-key.pem -out .leaf.csr \
  -subj "/CN=hive.local"
printf 'subjectAltName=DNS:hive.local,DNS:*.hive.local\n' > .leaf.ext
openssl x509 -req -in .leaf.csr -CA hive-ca.pem -CAkey .ca-key.pem \
  -CAcreateserial -days 36500 -sha256 -extfile .leaf.ext -out gateway-leaf.pem

cp hive-ca.pem trust-bundle.pem
rm -f .ca-key.pem .leaf-key.pem .leaf.csr .leaf.ext hive-ca.srl
```

They are dated 100 years out, so this is not a test that starts failing on a
Tuesday in 2027. `gio::TlsCertificate::from_file` parses without validating, so
expiry would not matter either way — but a fixture nobody has to think about is
worth the two characters.
