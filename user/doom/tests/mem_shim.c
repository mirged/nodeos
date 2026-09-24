/* Stands in for libnode/src/rt.rs, which owns these names in the real build. */
#include <stddef.h>
void *memcpy(void *d, const void *s, size_t n){unsigned char*a=d;const unsigned char*b=s;while(n--)*a++=*b++;return d;}
void *memmove(void *d, const void *s, size_t n){unsigned char*a=d;const unsigned char*b=s;if(a<b){while(n--)*a++=*b++;}else{a+=n;b+=n;while(n--)*--a=*--b;}return d;}
void *memset(void *d, int c, size_t n){unsigned char*a=d;while(n--)*a++=(unsigned char)c;return d;}
int memcmp(const void*x,const void*y,size_t n){const unsigned char*a=x,*b=y;while(n--){if(*a!=*b)return *a-*b;a++;b++;}return 0;}
size_t strlen(const char*s){const char*p=s;while(*p)p++;return p-s;}
