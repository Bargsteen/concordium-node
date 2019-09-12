#!/usr/bin/env bash

if [ "$#" -ne 1 ]
then
  echo "Usage: ./build-universal-docker.sh VERSION-TAG"
  exit 1
fi

export DOCKER_BUILDKIT=1

docker build -f scripts/universal.Dockerfile -t concordium/universal:$1 --ssh default .

docker tag concordium/universal:$1 192549843005.dkr.ecr.eu-west-1.amazonaws.com/concordium/universal:$1

echo "DONE BUILDING universal!"
