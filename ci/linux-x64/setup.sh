#!/bin/bash

RequiredRustToolchain="stable"

curl https://sh.rustup.rs -sSf | sh -s -- --default-host x86_64-unknown-linux-gnu --default-toolchain $RequiredRustToolchain -y

ls /home/appveyor
ls /home/appveyor/.cargo
