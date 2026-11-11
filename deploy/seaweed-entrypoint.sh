#!/bin/sh
set -eu
umask 077

# Host secrets stay 0600. Copy only this container's config for the image's
# unprivileged account before its normal entrypoint drops root privileges.
mkdir -p /run/artifactory-s3
chmod 700 /run/artifactory-s3
cp /etc/seaweed/s3.json /run/artifactory-s3/s3.json
chmod 600 /run/artifactory-s3/s3.json
chown -R seaweed:seaweed /run/artifactory-s3
exec /entrypoint.sh "$@"
