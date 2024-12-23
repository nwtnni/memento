// #pragma once

#include <libpmemobj++/p.hpp>
#include <libpmemobj++/persistent_ptr.hpp>
#include <libpmemobj++/make_persistent.hpp>
#include <libpmemobj++/pool.hpp>
#include <libpmemobj++/transaction.hpp>
#include <libpmemobj++/mutex.hpp>
#include <iostream>
#include "../common.hpp"

using namespace pmem::obj;
using namespace std;

#ifndef PMDK_QUEUE
#define PMDK_QUEUE

class queue
{
    struct node
    {
        p<int> value; // TODO: generic
        persistent_ptr<node> next;
    } __attribute((aligned(128)));

private:
    pmem::obj::mutex pmutex;
    persistent_ptr<node> head;
    persistent_ptr<node> tail;

public:
    void push(pool_base &pop, uint64_t value);
    std::optional<int> pop(pool_base &pop);
    void show(void) const;
};

#endif

#ifndef PMDK_QUEUE_TEST
#define PMDK_QUEUE_TEST

int get_queue_nops(string filepath, int nr_threads, float duration, int init, optional<int> prob);

#endif
