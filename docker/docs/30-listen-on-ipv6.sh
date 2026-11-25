#!/bin/sh
# Adds an IPv6 listener where the container has IPv6 (D-026). The image's
# own 10-listen-on-ipv6-by-default.sh edits only the packaged default.conf,
# and the root filesystem may be read-only, so this writes under /tmp,
# which nginx.conf includes.
set -eu
# procfs reports size 0, so read it rather than test -s.
if grep -q . /proc/net/if_inet6 2>/dev/null; then
    mkdir -p /tmp/nginx-listen
    echo 'listen [::]:8080;' > /tmp/nginx-listen/ipv6.conf
    echo "$0: listening on [::]:8080 as well"
fi
