#!/usr/bin/env bash

echo "Try the following in another terminal:"
echo '	echo -e "GET / HTTP/1.0\r\n\r\n" | ncat 127.0.0.1 8080'

export RUST_LOG="debug"

systemd-socket-activate --setenv=RUST_LOG --listen="127.0.0.1:8080" ./target/debug/echo-server activation-pong
