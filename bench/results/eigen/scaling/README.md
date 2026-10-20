# Steps against N (DESIGN.md §6.5, §12, §14 item 21)

`bench/eigen.py --graph {random,sbm} --nodes N --cases normalized_laplacian:smallest
--threads 4 --precision f32 --max-iter 100000` (gaps from a k + 4 ARPACK solve;
3M random not run). Built at 5611b0d (the later basis-pass levers are expected not to
change the step counts, not measured: the eight-lane dot products reassociate
sums, which can move a convergence test or a restart by a step or a restart), on a shared
4-vCPU Intel Xeon VM, 15 GiB, with another agent's pgrx tests alongside (load
average 4–8, recorded per timing): steps and matvecs are reliable, seconds are not.
