-- Per-actor Ed25519 key (PKCS#8 PEM). Advertised as `additionalPublicKeys`
-- and used for RFC 9421 signatures; the RSA key stays the primary
-- `publicKey`. Generated lazily for rows provisioned before this migration.
ALTER TABLE accounts ADD COLUMN ed25519_private_key_pem TEXT;
