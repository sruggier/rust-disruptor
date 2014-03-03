#!/bin/bash
rustc --test -o UnicastThroughputTest_test UnicastThroughputTest.rs &&
rustc --opt-level 2 -g UnicastThroughputTest.rs
