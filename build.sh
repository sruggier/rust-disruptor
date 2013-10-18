#!/bin/bash
rustc --test -o UnicastThroughputTest_test UnicastThroughputTest.rs &&
rustc --opt-level 2 -Z extra-debug-info UnicastThroughputTest.rs
