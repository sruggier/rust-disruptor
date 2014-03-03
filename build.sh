#!/bin/bash
rustc --test -A dead-code -o UnicastThroughputTest_test UnicastThroughputTest.rs &&
rustc  -A dead-code --opt-level 2 -Z lto -g UnicastThroughputTest.rs
