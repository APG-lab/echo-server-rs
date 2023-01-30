#!/usr/bin/env bash

echo "Try the following in another terminal:"
echo '	echo -e "GET / HTTP/1.0\r\n\r\n" | ncat --unixsock foo.sock'

export RUST_LOG="debug"

rm -f foo.sock
./target/debug/echo-server unix-pong foo.sock
