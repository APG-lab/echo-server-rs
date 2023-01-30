#!/usr/bin/env bash

echo "Try the following in another terminal:"
echo '	echo -e "GET / HTTP/1.0\r\n\r\n" | ncat 127.0.0.1 8080'

trap "trap - SIGTERM && kill -- -$$" SIGINT SIGTERM EXIT

export RUST_LOG="debug"

rm -f foo.sock
./target/debug/echo-server unix-pong foo.sock &

systemd-socket-activate --setenv=RUST_LOG --listen="127.0.0.1:8080" ./target/debug/echo-server activation-proxy-unix foo.sock 
