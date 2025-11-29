#!/bin/sh

CERT_PATH="/etc/nginx/ssl/nginx.crt"
KEY_PATH="/etc/nginx/ssl/nginx.key"

# Create directory if missing
mkdir -p /etc/nginx/ssl

# If cert/key do not exist, generate them
if [ ! -f "$CERT_PATH" ] || [ ! -f "$KEY_PATH" ]; then
    echo "Generating self-signed SSL certificate..."

    openssl req -x509 \
        -newkey rsa:2048 \
        -nodes \
        -keyout "$KEY_PATH" \
        -out "$CERT_PATH" \
        -days 365 \
        -subj "/CN=localhost"
fi

exec "$@"
