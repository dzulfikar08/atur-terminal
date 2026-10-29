#!/bin/bash
# Runs the echo bench program on the emulator, against the server on the host (10.0.2.2).
#
#   echo-floor-emulator.sh <bench-binary> <link-file> [variants]
set -uo pipefail

bench="$1"
link="$2"
variants="${3:-tls,tls+type+paste,auto}"
adb wait-for-device
adb shell getprop ro.product.cpu.abi
adb push "$bench" /data/local/tmp/echo_bench
adb push "$link" /data/local/tmp/link.txt
adb shell chmod 755 /data/local/tmp/echo_bench
adb shell "/data/local/tmp/echo_bench /data/local/tmp/link.txt $variants" 2>&1 | tee emulator-bench.txt
