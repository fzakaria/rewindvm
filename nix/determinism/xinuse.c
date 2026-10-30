/* Prints XINUSE, which xgetbv(1) reads: which parts of the extended
 * register state are in use. The CPU model does not advertise
 * XGETBV_ECX1, but the instruction runs in the guest anyway. Before and
 * after touching the AVX registers, after a sleep, which lets the
 * kernel switch tasks and restore the state, and after a vzeroall and a
 * fork, which make the kernel save the state and restore it. */
#include <stdint.h>
#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>

static uint32_t xinuse(void)
{
  uint32_t lo, hi;
  __asm__ volatile("xgetbv" : "=a"(lo), "=d"(hi) : "c"(1));
  return lo;
}

int main(void)
{
  printf("xinuse at start: %#x\n", xinuse());
  __asm__ volatile("vpxor %%ymm1, %%ymm1, %%ymm1" ::: "xmm1");
  __asm__ volatile("vpcmpeqd %%ymm2, %%ymm2, %%ymm2" ::: "xmm2");
  printf("xinuse after AVX: %#x\n", xinuse());
  usleep(1000);
  printf("xinuse after a sleep: %#x\n", xinuse());

  /* vzeroall zeroes every vector register, and an Intel performance core
   * then counts SSE as not in use. The fork saves this task's state, and
   * the child running in between makes the kernel restore it from what
   * it saved. XINUSE is not printed between the two: what it reads
   * right after vzeroall depends on the kind of core. */
  fflush(stdout);
  __asm__ volatile("vzeroall" ::: "xmm0", "xmm1", "xmm2", "xmm3", "xmm4",
                   "xmm5", "xmm6", "xmm7", "xmm8", "xmm9", "xmm10", "xmm11",
                   "xmm12", "xmm13", "xmm14", "xmm15");
  if (fork() == 0) {
    _exit(0);
  }
  wait(NULL);
  printf("xinuse after vzeroall and a fork: %#x\n", xinuse());
  return 0;
}
