/* Draws a hardware random number with rdrand and keeps it in a global,
 * sleeps 1500 times, and prints a fixed word: nothing the program does
 * after the draw depends on the number, so only the machine's memory
 * differs between two runs. A sleep is about 11 steps, and keyframes come
 * 10,000 to 13,000 steps of them apart after the first at step 256, so
 * the sleeps reach a keyframe after the draw, where a replay sees the
 * difference. */
#include <immintrin.h>
#include <stdio.h>
#include <unistd.h>

unsigned long long secret;

int main(void)
{
  unsigned long long v = 0;
  _rdrand64_step(&v);
  secret = v;
  for (int i = 0; i < 1500; i++) {
    usleep(1000);
  }
  printf("done\n");
  return 0;
}
