#!/usr/bin/env bash

GROUP_ID=""
USER_ID=""

if [ ! -z "$EXTERNAL_GID" ];
then
  GROUP_ID="$EXTERNAL_GID"
else
  GROUP_ID="61000"
fi

if [ ! -z "$EXTERNAL_UID" ];
then
  USER_ID="$EXTERNAL_UID"
else
  USER_ID="61000"
fi

groupadd -g $GROUP_ID docker
useradd -g $GROUP_ID -l -m -s /bin/false -u $USER_ID docker

mkdir -p /var/lib/concordium
chown -R $USER_ID:$GROUP_ID /var/lib/concordium

if [ -f /var/lib/concordium/data/baker-0-account.json ] && [ -f /var/lib/concordium/data/baker-0-credentials.json ];
then
  export BAKER_ID=node-0
fi

/usr/bin/supervisord -c /etc/supervisor/supervisord.conf