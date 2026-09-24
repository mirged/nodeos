/* Host test for the mini-libc.
 *
 * The libc under `user/doom/libc/` is hand-written and Doom leans on it for
 * things that fail quietly when they are wrong -- a sscanf that does not
 * assign, a printf width that is off by one, a realloc that loses the old
 * contents. Booting the whole system to find that out is a slow way to learn
 * it, so this links the same sources for the host, freestanding, with the
 * ndos_* platform functions implemented as raw Linux syscalls, and checks
 * them directly.
 *
 * Run it with `make test-doom-libc`. It is not part of the NodeOS image.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "nodeos_platform.h"

static long sys(long n, long a, long b, long c) {
    long r; __asm__ volatile("syscall" : "=a"(r) : "a"(n),"D"(a),"S"(b),"d"(c) : "rcx","r11","memory"); return r;
}
void ndos_write(const char *b, size_t n) { sys(1, 1, (long)b, (long)n); }
void ndos_exit(int c) { sys(60, c, 0, 0); __builtin_unreachable(); }
uint64_t ndos_time_ns(void) { return 0; }
void ndos_yield(void) {}
const unsigned char *ndos_wad_data(size_t *l) { if(l)*l=0; return 0; }
uint8_t *ndos_fb_info(uint32_t*w,uint32_t*h,uint32_t*p){(void)w;(void)h;(void)p;return 0;}
size_t ndos_kbd_read(unsigned char*b,size_t n){(void)b;(void)n;return 0;}
static char heap[8u<<20]; static size_t heap_used;
void *ndos_sbrk(size_t n) { if (heap_used+n > sizeof heap) return 0; void*p=heap+heap_used; heap_used+=n; return p; }

static int fails;
#define CHECK(cond, ...) do { if (!(cond)) { fails++; printf("FAIL %s:%d: ", __FILE__, __LINE__); printf(__VA_ARGS__); putchar('\n'); } } while (0)

static void check_str(const char *got, const char *want, const char *what) {
    if (strcmp(got, want) != 0) { fails++; printf("FAIL %s: got \"%s\" want \"%s\"\n", what, got, want); }
}

void test(void) {
    char b[128];

    /* printf */
    snprintf(b,sizeof b,"%d %i %u %x %X %o",-42,7,4294967295u,255,255,8); check_str(b,"-42 7 4294967295 ff FF 10","ints");
    snprintf(b,sizeof b,"[%5d][%-5d][%05d]",42,42,42); check_str(b,"[   42][42   ][00042]","width");
    snprintf(b,sizeof b,"[%s][%8s][%-8s][%.3s]","ab","ab","ab","abcdef"); check_str(b,"[ab][      ab][ab      ][abc]","strings");
    snprintf(b,sizeof b,"%c%%%p",'x',(void*)0x1234); check_str(b,"x%0x1234","char/pct/ptr");
    snprintf(b,sizeof b,"%ld %lld %zu",-1L,-1LL,(size_t)5); check_str(b,"-1 -1 5","lengths");
    CHECK(snprintf(b,4,"abcdef")==6,"snprintf returns full length");
    check_str(b,"abc","snprintf truncation");

    /* sscanf: exactly what M_StrToInt does */
    int v;
    CHECK(sscanf(" 0x1f"," 0x%x",&v)==1 && v==31,"hex got %d",v);
    CHECK(sscanf(" 0X2A"," 0X%x",&v)==1 && v==42,"HEX got %d",v);
    CHECK(sscanf(" 017"," 0%o",&v)==1 && v==15,"octal got %d",v);
    CHECK(sscanf(" -99"," %d",&v)==1 && v==-99,"dec got %d",v);
    CHECK(sscanf("abc"," %d",&v)==0,"non-numeric must not assign");
    CHECK(sscanf("0x20","%i",&v)==1 && v==32,"%%i hex got %d",v);
    CHECK(sscanf("12 34","%d %d",&v,&v)==2,"two conversions");
    char word[16];
    CHECK(sscanf("  hello world","%s",word)==1 && strcmp(word,"hello")==0,"%%s got %s",word);

    /* strtol */
    CHECK(strtol("  -0x1A",0,16)==-26,"strtol hex");
    CHECK(strtol("077",0,0)==63,"strtol auto octal");
    CHECK(atoi("  123abc")==123,"atoi");

    /* strings */
    check_str(strstr("foobar","oba"),"obar","strstr");
    CHECK(strcasecmp("AbC","aBc")==0,"strcasecmp");
    CHECK(strncasecmp("AbCx","aBcy",3)==0,"strncasecmp");
    strcpy(b,"ab"); strlcat(b,"cdef",5); check_str(b,"abcd","strlcat truncates");
    CHECK(strlcpy(b,"abcdef",4)==6,"strlcpy returns source length");
    check_str(b,"abc","strlcpy truncates");
    check_str(strrchr("a/b/c",'/'),"/c","strrchr");

    /* malloc/realloc/free */
    void *p1=malloc(100), *p2=malloc(100);
    CHECK(p1&&p2&&p1!=p2,"distinct allocations");
    memset(p1,0xAB,100); free(p1);
    void *p3=malloc(64); CHECK(p3!=0,"reuse after free");
    char *g=malloc(10); strcpy(g,"hi"); g=realloc(g,200);
    CHECK(g&&strcmp(g,"hi")==0,"realloc preserves contents");
    free(g); free(p2); free(p3);
    void *big=malloc(3u<<20); CHECK(big!=0,"multi-MiB allocation");
    memset(big,0,3u<<20); free(big);

    /* stdio over the in-memory files */
    FILE *f=fopen("save.dsg","wb"); CHECK(f!=0,"create for write");
    CHECK(fwrite("hello",1,5,f)==5,"fwrite"); fclose(f);
    f=fopen("save.dsg","rb"); CHECK(f!=0,"reopen for read");
    memset(b,0,sizeof b); CHECK(fread(b,1,5,f)==5,"fread"); check_str(b,"hello","file contents");
    CHECK(fseek(f,0,SEEK_END)==0 && ftell(f)==5,"seek/tell"); fclose(f);
    CHECK(fopen("missing.cfg","rb")==0,"missing file");

    printf(fails ? "libc: %d FAILURES\n" : "libc: all checks passed (%d failures)\n", fails);
}
void _start(void) { test(); ndos_exit(fails ? 1 : 0); }
