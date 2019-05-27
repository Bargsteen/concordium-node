#!/bin/bash

# Haskell binding needs proper library path to function
export LD_LIBRARY_PATH=/usr/local/lib

ARGS=""

# Determine what arguments to pass to the binary

if [ -n "$ID" ];
then
    ARGS="$ARGS --id $ID"
fi

if [ -n "$LISTEN_PORT" ];
then
    ARGS="$ARGS --listen-port $LISTEN_PORT"
fi

if [ -n "$DESIRED_PEERS" ];
then
    ARGS="$ARGS --desired-nodes $DESIRED_PEERS"
fi

if [ -n "$BAKER_ID" ];
then
    ARGS="$ARGS --baker-id $(echo $BAKER_ID | cut -d'-' -f2)"
fi

if [ -n "$PROMETHEUS_METRICS_SERVER" ];
then
    ARGS="$ARGS --prometheus-server"
fi

if [ -n "$PROMETHEUS_METRICS_PORT" ];
then
    ARGS="$ARGS --prometheus-listen-port $PROMETHEUS_METRICS_PORT"
fi

if [ -n "$PROMETHEUS_METRICS_IP" ];
then
    ARGS="$ARGS --prometheus-listen-addr $PROMETHEUS_METRICS_IP"
fi

if [ -n "$CONFIG_DIR" ];
then
    ARGS="$ARGS --override-config-dir $CONFIG_DIR"
    mkdir -p $CONFIG_DIR
fi

if [ -n "$DATA_DIR" ];
then
    ARGS="$ARGS --override-data-dir $DATA_DIR"
    mkdir -p $DATA_DIR
    cd $DATA_DIR
fi

if [ -n "$NUM_BAKERS" ];
then
    ARGS="$ARGS --num-bakers $NUM_BAKERS"
    if [ -n "$DATA_DIR" ];
    then
        cd /build-project/genesis-data
        tar -xvf $NUM_BAKERS-bakers.tar.gz
        cd genesis_data/
        cp * $DATA_DIR/
        cd $DATA_DIR
    fi
fi

if [ -n "$BOOTSTRAP_FIRST_NODE" ];
then
    ARGS="$ARGS --bootstrap-node $BOOTSTRAP_FIRST_NODE"
fi

if [ -n "$BOOTSTRAP_SECOND_NODE" ];
then
    ARGS="$ARGS --bootstrap-node $BOOTSTRAP_SECOND_NODE"
fi

if [ -n "$RPC_SERVER_ADDR" ];
then
    ARGS="$ARGS --rpc-server-addr $RPC_SERVER_ADDR"
fi

if [ -n "$TPS_MESSAGE_COUNT" ];
then
    ARGS="$ARGS --tps-message-count $TPS_MESSAGE_COUNT"
fi

if [ -n "$TPS_RECEIVER_ID" ];
then
    ARGS="$ARGS --tps-test-recv-id $TPS_RECEIVER_ID"
fi

if [ -n "$MAX_NODES" ];
then
    ARGS="$ARGS --max-nodes $MAX_NODES"
fi

if [ -n "$LISTEN_HTTP_PORT" ];
then
    ARGS="$ARGS --listen-http-port $LISTEN_HTTP_PORT"
fi

if [ -n "$MAX_ALLOWED_NODES" ];
then
    ARGS="$ARGS --max-allowed-nodes $MAX_NODES"
fi

if [ -n "$MAX_ALLOWED_NODES_PERCENTAGE" ];
then
    ARGS="$ARGS --max-allowed-nodes-percentage $MAX_NODES_PERCENTAGE"
fi

if [ -n "$EXTRA_ARGS" ];
then
    ARGS="$ARGS $EXTRA_ARGS"
fi

if [ -n "$ARTIFICIAL_DELAY" ];
then
    sleep $ARTIFICIAL_DELAY
fi


if [ "$MODE" == "tps_receiver" ]; then
    echo "Receiver!"
    
    /build-project/p2p_client-cli \
    --enable-tps-test-recv \
    --external-ip 10.96.0.15 \
    $ARGS

elif [ "$MODE" == "tps_sender" ]; then
	echo "Sender!\n"
    
    mkdir -p $DATA_DIR/tps_test

    echo "Generating data\n"
    cd $DATA_DIR/tps_test

    for i in `seq 0 $(($TPS_MESSAGE_COUNT - 1))`;
    do
	    echo $i
	    dd if=/dev/urandom of=test-$i bs=1 count=1024 > /dev/null 2>&1
    done

    # Echo to cron file

    /build-project/p2p_client-cli \
    --tps-test-data-dir $DATA_DIR/tps_test \
    --baker-id 1 \
    --connect-to 10.96.0.15:8888 \
    --external-ip 10.96.0.16 \
    $ARGS
elif [ "$MODE" == "basic" ]; then   
    /build-project/p2p_client-cli $ARGS
elif [ "$MODE" == "bootstrapper" ]; then
    /build-project/p2p_bootstrapper-cli $ARGS
elif [ "$MODE" == "testrunner" ]; then  
    /build-project/testrunner $ARGS 

elif [ "$MODE" == "local_basic" ]; then
    export BAKER_ID=`curl http://baker_id_gen:8000/next_id`
    echo "Using BAKER_ID $BAKER_ID"


    /build-project/p2p-client/target/debug/p2p_client-cli \
        --no-dnssec \
        --testrunner-url http://testrunner:8950 \
        --desired-nodes $DESIRED_PEERS \
        --external-port $EXTERNAL_PORT \
        --bootstrap-node $BOOTSTRAP_NODE \
        --baker-id $BAKER_ID \
        --num-bakers $NUM_BAKERS \
        --rpc-server-addr 0.0.0.0 \
        $EXTRA_ARGS

elif [ "$MODE" == "local_bootstrapper" ]; then
    export NODE_ID=`awk 'END{ print $1}' /etc/hosts | sha256sum | awk '{ print $1 }' | cut -c1-16`
    /build-project/p2p-client/target/debug/p2p_bootstrapper-cli \
        --id $NODE_ID \
        --listen-port 8888 \
        $EXTRA_ARGS

elif [ "$MODE" == "local_testrunner" ]; then
    /build-project/p2p-client/target/debug/testrunner \
        --no-dnssec \
        --desired-nodes $DESIRED_PEERS \
        --external-port $EXTERNAL_PORT \
        --bootstrap-node $BOOTSTRAP_NODE \
        $EXTRA_ARGS
else
    echo "No matching MODE was found. Please check!"
fi
