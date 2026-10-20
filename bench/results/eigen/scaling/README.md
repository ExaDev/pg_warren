# Steps against N (DESIGN.md §6.5, §12, §14 item 21)

`bench/eigen.py --graph {random,sbm} --nodes N --cases normalized_laplacian:smallest
--threads 4 --precision f32 --max-iter 100000` (gaps from a k + 4 ARPACK solve;
3M random not run). Built at 5611b0d (the step counts do not depend on the later
basis-pass levers; their eigenvalues differ only in the last bits), on a shared
4-vCPU Intel Xeon VM, 15 GiB, with another agent's pgrx tests alongside (load
average 4–8, recorded per timing): steps and matvecs are reliable, seconds are not.
