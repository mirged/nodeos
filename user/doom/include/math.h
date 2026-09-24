/* Doom is fixed-point throughout: every <math.h> use in the tree we build is
   inside `#if 0` blocks kept from the 1997 release. The declarations exist so
   those files still compile; nothing references them, and the port is built
   with -mgeneral-regs-only, so a real call would fail to link rather than
   silently pull in a soft-float runtime. */
#ifndef _MATH_H
#define _MATH_H
#define PI 3.141592657
double sin(double x);
double cos(double x);
double tan(double x);
double atan(double x);
double atan2(double y, double x);
double sqrt(double x);
double pow(double b, double e);
double fabs(double x);
double floor(double x);
double ceil(double x);
#endif
