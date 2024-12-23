#!/bin/bash

export LD_LIBRARY_PATH=./hash/Dash/pmdk/src/nondebug:$LD_LIBRARY_PATH # to evaluate Dash
git_hash=$(git log -1 --format="%h")
git_date=$(git log -1 --date=format:'%Y%m%d' --format=%cd)

BIN="bin"
OUT="out"
OUT_DEBUG=./$OUT/debug.out

mkdir -p /mnt/pmem0/eval_hash
mkdir -p out

function dmsg() {
    msg=$1
    time=$(date +%m)/$(date +%d)-$(date +%H):$(date +%M)
    echo "[$time] $msg" >> $OUT_DEBUG
}

function bench() {
    target=$1   # possible arg: CCEH, Level, Dash, PCLHT, SOFT, clevel, clevel_rust, SOFT_rust (TODO: clevel_rust -> clevel_memento)
    workload=$2 # possible arg: insert, pos_search, ...
    mode=$3     # possible arg: THROUGHPUT, LOAD_FACTOR, RESIZE, LATENCY
    dist=$4     # possible arg: UNIFORM, SELFSIMILAR, ZIPFIAN
    thread=$5

    # set output
    out_dir=./$OUT/$mode/$dist/$workload
    mkdir -p $out_dir
    out=$out_dir/${target}_${git_hash}_${git_date}.out
    echo "out: $out"

    # make dummy folder for target except for clevel, clevel-rust
    if [[ "$target" != "clevel" && "$target" != "clevel_rust" ]]; then
        mkdir /mnt/pmem0/pibench
    fi

    # set workload parameters
    HASH_SIZE=16777216      # Initial capacity of hash table
    OP=200000000            # op num for each Load, Run phase
    SKIP_LOAD=false         # skip Load phase or not
    READ_RT=0               # rate of read operation (Run phase)
    INSERT_RT=1             # rate of insert operation (Run phase)
    REMOVE_RT=0             # rate of read operation (Run phase)
    NEGATIVE_RT=0           # rate of negative read among reads (Run phase)
    DISTRIBUTION=$dist      # Key distribution

    if [ "${workload}" == "insert" ]; then
        # Load 0M, Run 200M, Insert 100%
        SKIP_LOAD=true
    elif [ "${workload}" == "pos_search" ]; then
        # Load 200M, Run 200M, Read 100%, Negative 0%
        READ_RT=1
        INSERT_RT=0
        REMOVE_RT=0
    elif [ "${workload}" == "neg_search" ]; then
        # Load 200M, Run 200M, Read 100%, Negative 100%
        READ_RT=1
        INSERT_RT=0
        REMOVE_RT=0
        NEGATIVE_RT=1
    elif [ "${workload}" == "delete" ]; then
        # Load 200M, Run 200M, Delete 100%
        READ_RT=0
        INSERT_RT=0
        REMOVE_RT=1
    elif [ "${workload}" == "write_heavy" ]; then
        # Load 200M, Run 200M, Insert 80%, Read 20%
        READ_RT=0.2
        INSERT_RT=0.8
        REMOVE_RT=0
    elif [ "${workload}" == "balanced" ]; then
        # Load 200M, Run 200M, Insert 50%, Read 50%
        READ_RT=0.5
        INSERT_RT=0.5
        REMOVE_RT=0
    elif [ "${workload}" == "read_heavy" ]; then
        # Load 200M, Run 200M, Insert 20%, Read 80%
        READ_RT=0.8
        INSERT_RT=0.2
        REMOVE_RT=0
    elif [ "${workload}" == "dummy" ]; then
        HASH_SIZE=1
        OP=1
    else
        echo "invalid workload"
        exit
    fi

    # start evaluation
    echo "start target: $target, workload: $workload, mode: $mode, dist: $dist, thread: $thread"
    dmsg  "start target: $target, workload: $workload, mode: $mode, dist: $dist, thread: $thread"

    # NUMA node 0 pinning
    cmd="numactl --cpunodebind=0 --membind=0 ./$BIN/PiBench ./$BIN/$target.so \
        -S $HASH_SIZE \
        -p $OP \
        --skip_load=$SKIP_LOAD \
        -r $READ_RT -i $INSERT_RT -d $REMOVE_RT \
        -N $NEGATIVE_RT \
        -M $mode --distribution $DISTRIBUTION \
        -t $thread"
    echo $cmd
    $cmd >> $out

    ext=$?
    if [ $ext -ne 0 ]; then
        dmsg "exit with code $ext! (target: $target, workload: $workload, mode: $mode, dist: $dist, thread: $thread)"
    fi
    echo -e "\n\n" >> $out
}

function bench_all() {
    workload=$1 # possible arg: insert, pos_search, ...
    mode=$2     # possible arg: THROUGHPUT, LOAD_FACTOR, RESIZE, LATENCY
    dist=$3     # possible arg: UNIFORM, SELFSIMILAR, ZIPFIAN

    for THREAD in 1 4 8 16 24 32 48 64; do
        # LATENCY use only 32 thread
        if [ "$mode" == "LATENCY" ]; then
            THREAD=32
        fi

        bench clevel_rust $workload $mode $dist $THREAD
        bench clevel $workload $mode $dist $THREAD
        # bench CCEH $workload $mode $dist $THREAD
        # bench Level $workload $mode $dist $THREAD
        # bench Dash $workload $mode $dist $THREAD
        # bench PCLHT $workload $mode $dist $THREAD
        # bench SOFT $workload $mode $dist $THREAD
        # bench SOFT_rust $workload $mode $dist $THREAD

        # LATENCY use only 32 thread
        if [ "$mode" == "LATENCY" ]; then
            break
        fi
    done
}

dmsg "start run.sh (git hash: $git_hash, date: $git_date)"

# Fig 4, 5. Throughput
dmsg "start throughput with uniform distribution."
bench_all insert THROUGHPUT UNIFORM
bench_all pos_search THROUGHPUT UNIFORM
bench_all neg_search THROUGHPUT UNIFORM
bench_all delete THROUGHPUT UNIFORM
bench_all write_heavy THROUGHPUT UNIFORM
bench_all balanced THROUGHPUT UNIFORM
bench_all read_heavy THROUGHPUT UNIFORM
dmsg "throughput with uniform distribution was done."
dmsg "start throughput with self-similar distribution."
bench_all insert THROUGHPUT SELFSIMILAR
bench_all pos_search THROUGHPUT SELFSIMILAR
bench_all neg_search THROUGHPUT SELFSIMILAR
bench_all delete THROUGHPUT SELFSIMILAR
bench_all write_heavy THROUGHPUT SELFSIMILAR
bench_all balanced THROUGHPUT SELFSIMILAR
bench_all read_heavy THROUGHPUT SELFSIMILAR
dmsg "throughput with self-similar distribution was done."
dmsg "all throughput was done."

# Fig 7. Latency
# dmsg "start latency with uniform distribution."
# bench_all insert LATENCY UNIFORM
# bench_all pos_search LATENCY UNIFORM
# bench_all neg_search LATENCY UNIFORM
# bench_all delete LATENCY UNIFORM
# dmsg "latency with uniform distribution was done."
# dmsg "all latency was done."

# plot
dmsg "plotting.."
python3 plot.py

dmsg "all work is done!"
