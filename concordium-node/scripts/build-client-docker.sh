#!/usr/bin/env bash

if [ "$#" -ne 1 ]
then
  echo "Usage: ./build-client-docker.sh VERSION-TAG"
  exit 1
fi

sed -i "s/VERSION_TAG/$1/" scripts/client.Dockerfile

docker build -f scripts/client.Dockerfile -t 192549843005.dkr.ecr.eu-west-1.amazonaws.com/concordium/client:$1 .

docker push 192549843005.dkr.ecr.eu-west-1.amazonaws.com/concordium/client:$1