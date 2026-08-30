#!/usr/bin/env bash
# Bench service launcher (runs inside a tmux pane so Ctrl+C reaches it as a
# console event for the graceful-shutdown path; lib.rs console_shutdown).
cd /e/Rs_Codes/rs-iot || exit 1
exec ./target/release/rs-iot --config bench/history-100k/config.toml
