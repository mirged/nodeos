#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
static pthread_mutex_t m = PTHREAD_MUTEX_INITIALIZER; static long total;
static void *work(void *a) { for (int i = 0; i < 100000; i++) { pthread_mutex_lock(&m); total++; pthread_mutex_unlock(&m);} return a; }
int main(void) { pthread_t t[4]; for (int i=0;i<4;i++) pthread_create(&t[i],0,work,0); for(int i=0;i<4;i++) pthread_join(t[i],0);
  char *p = malloc(1<<20); memset(p,1,1<<20); free(p); struct timespec ts; clock_gettime(CLOCK_REALTIME,&ts);
  printf("total %ld %f\n", total, ts.tv_sec/1e9); return 0; }
