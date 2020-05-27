#/bin/bash
DOCKER_BUILDKIT=1 docker build -f Dockerfile-amd64 -t biboop --output target/release .
