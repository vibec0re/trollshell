#!/usr/bin/env bash
# Regenerate the TLS fixtures `src/verify.rs`'s and `src/tls.rs`'s gated tests
# run against — a local `GTlsServerConnection` on one side and the window's own
# `verify::probe` on the other (#1234 ask 3).
#
# Run it from this directory, inside the devShell (openssl is there; it is not
# on the bare PATH):
#
#     nix develop --command bash crates/trollshell-agent-window/tests/fixtures/tls/generate.sh
#
# NOTHING HERE IS SECRET and nothing here is trusted by anything anywhere.
# `hive.local` is not a resolvable name, the CA keys are deleted at the end,
# and the three server keys that stay are 2048-bit throwaways whose only job is
# to let a test bind a TLS listener on 127.0.0.1. Checking them in is
# deliberate: the alternative is minting certificates inside the nix check
# sandbox, which would put `openssl` in that closure and make a test's inputs
# depend on the clock.
#
# The leaves carry **IP:127.0.0.1** as well as DNS:hive.local so a test can
# connect to the loopback address it just bound and still pass GIO's identity
# check — that check is half of what these fixtures exist to exercise.
set -euo pipefail
cd "$(dirname "$0")"

DAYS=36500 # a century: a fixture nobody has to think about again

mint_ca() {
	# $1 = output cert, $2 = output key, $3 = CN
	openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days "$DAYS" \
		-keyout "$2" -out "$1" -subj "/CN=$3" \
		-addext "basicConstraints=critical,CA:TRUE,pathlen:0" \
		-addext "keyUsage=critical,keyCertSign,cRLSign"
}

mint_leaf() {
	# $1 = output cert, $2 = output key, $3 = CA cert, $4 = CA key, $5.. = extra x509 args
	local out="$1" key="$2" ca="$3" cakey="$4"
	shift 4
	openssl req -newkey rsa:2048 -nodes -keyout "$key" -out .leaf.csr -subj "/CN=hive.local"
	printf 'subjectAltName=DNS:hive.local,DNS:*.hive.local,DNS:localhost,IP:127.0.0.1,IP:::1\n' >.leaf.ext
	printf 'basicConstraints=critical,CA:FALSE\n' >>.leaf.ext
	printf 'keyUsage=critical,digitalSignature,keyEncipherment\n' >>.leaf.ext
	printf 'extendedKeyUsage=serverAuth\n' >>.leaf.ext
	openssl x509 -req -in .leaf.csr -CA "$ca" -CAkey "$cakey" -CAcreateserial \
		-sha256 -extfile .leaf.ext -out "$out" "$@"
}

mint_ca fixture-ca.pem .fixture-ca-key.pem "hive-ca hive.local (trollshell test fixture)"
mint_ca .other-ca.pem .other-ca-key.pem "other-ca (trollshell test fixture)"

# The gateway's real leaf, and one for the same name under a CA the bundle does
# not carry.
mint_leaf server-leaf.pem server-leaf-key.pem fixture-ca.pem .fixture-ca-key.pem -days "$DAYS"
mint_leaf other-leaf.pem other-leaf-key.pem .other-ca.pem .other-ca-key.pem -days "$DAYS"

# An expired leaf under the RIGHT CA, so the only thing wrong with it is the
# clock. Fixed absolute dates, so it is expired for ever rather than from some
# Tuesday onwards.
mint_leaf expired-leaf.pem expired-leaf-key.pem fixture-ca.pem .fixture-ca-key.pem \
	-not_before 20200101000000Z -not_after 20210101000000Z

# hyperhive's own `gateway.pem` shape: `cat leaf-only ca.pem`, LEAF FIRST with
# the CA appended (hive-tls.nix; @the-sword-above on #1224, 12:14Z). The window
# pins the first block, so the order is the whole contract.
cat server-leaf.pem fixture-ca.pem >gateway.pem
# …and the same two certificates the wrong way round, which is what a
# trust-bundle looks like and what the card must be able to complain about.
cat fixture-ca.pem server-leaf.pem >gateway-ca-first.pem

rm -f .leaf.csr .leaf.ext .fixture-ca-key.pem .other-ca-key.pem .other-ca.pem \
	fixture-ca.srl .other-ca.srl
echo "regenerated: $(ls ./*.pem | tr '\n' ' ')"
