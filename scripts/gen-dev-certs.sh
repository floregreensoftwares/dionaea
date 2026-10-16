#!/bin/sh
# Makes a throwaway CA and a server certificate for 127.0.0.1 and localhost in
# .dev-certs/, for the TLS variant of the development ClickHouse
# (compose.tls.yaml). Development only: the key is world readable so the
# container's user can read it.
set -eu
# Git for Windows rewrites arguments that start with a slash, such as -subj /CN=x.
export MSYS_NO_PATHCONV=1

dir=${1:-.dev-certs}
mkdir -p "$dir"
cd "$dir"
rm -f ca.pem ca.key server.pem server.key server.csr server.ext ca.srl

openssl ecparam -name prime256v1 -genkey -noout -out ca.key
openssl req -x509 -new -key ca.key -sha256 -days 30 -subj "/CN=dionaea dev CA" \
    -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" \
    -out ca.pem

openssl ecparam -name prime256v1 -genkey -noout -out server.key
openssl req -new -key server.key -subj "/CN=localhost" -out server.csr
cat > server.ext <<'EXT'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=serverAuth
subjectAltName=DNS:localhost,IP:127.0.0.1
EXT
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -sha256 -days 30 \
    -extfile server.ext -out server.pem

rm -f server.csr server.ext ca.srl
chmod 644 ca.pem server.pem server.key
echo "wrote $dir/ca.pem, $dir/server.pem and $dir/server.key"
