#!/bin/bash
if [ "$#" -ne 1 ]
then
  echo "Usage: ./build-all-docker.sh VERSION-TAG"
  exit 1
fi

scripts/build-base-docker.sh
scripts/build-build-docker.sh $1
scripts/build-bootstrapper-docker.sh $1
scripts/build-basic-docker.sh $1
scripts/build-ipdiscovery-docker.sh $1