
# Evaluation of "A General Framework for Detectable, Persistent, Lock-Free Data Structures" (§6)

We implemented several lock-free data structures based on memento, and evaluate their correcntess and performance. To reproduce our results, please see the documentation for each data structrue. We assume you mount your PMEM at `/mnt/pmem0/`.

- [Detecability evaluation](./correctness/README.md) (§6.1)
- Performance evaluation (§6.2)
  - [Detectable CAS](./performance/cas/README.md)
  - [Detectable List](./performance/list/README.md)
  - [Detectable Queue](./performance/queue/README.md)
  - [Detectable Hash Table](./performance/hash/README.md)
