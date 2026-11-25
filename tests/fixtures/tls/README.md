Self-signed certificates for `localhost` used only by the listener tests
(`src/listeners.rs`, `tests/protocols_integration.rs`). They are trusted
nowhere, guard nothing, and are committed so the tests need no certificate
generator. `renewed.*` stands in for a renewed certificate.

Made with:

    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
      -keyout localhost.key -out localhost.crt -days 36500 -subj /CN=localhost \
      -addext subjectAltName=DNS:localhost,IP:127.0.0.1 \
      -addext basicConstraints=critical,CA:FALSE -addext keyUsage=critical,digitalSignature \
      -addext extendedKeyUsage=serverAuth
