/* A first-fit heap over ndos_sbrk.
 *
 * Doom hardly uses malloc: Z_Init takes one large block and every later
 * allocation is carved out of that zone by z_zone.c. What is left is a few
 * dozen strdup/config allocations. So this is deliberately the simplest
 * allocator that does not leak: a singly linked list of blocks, first fit,
 * with forward coalescing on free and splitting on allocation.
 */
#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include <stdlib.h>

#include "nodeos_platform.h"

#define ALIGN       16
#define CHUNK_BYTES (1u << 20) /* how much to ask the kernel for at a time */

typedef struct block
{
    size_t size;         /* usable bytes after this header */
    struct block *next;  /* insertion order, not address order */
    int free;
    int _pad;
} block_t;

static block_t *heap_head;

static size_t align_up(size_t v, size_t to)
{
    return (v + to - 1) & ~(to - 1);
}

/* Ask the platform for more address space and return it as one free block
   linked at the end of the list.

   Separate calls to ndos_sbrk are separate memory objects and need not be
   adjacent, so the list is in insertion order rather than address order.
   Coalescing checks adjacency explicitly, which means an out-of-order seam
   costs a missed merge and never a wrong answer. */
static block_t *grow(size_t need)
{
    size_t want = align_up(need + sizeof(block_t), CHUNK_BYTES);
    block_t *fresh = ndos_sbrk(want);

    if (fresh == NULL)
    {
        return NULL;
    }
    fresh->size = want - sizeof(block_t);
    fresh->next = NULL;
    fresh->free = 1;

    if (heap_head == NULL)
    {
        heap_head = fresh;
    }
    else
    {
        block_t *tail = heap_head;
        while (tail->next != NULL)
        {
            tail = tail->next;
        }
        tail->next = fresh;
        /* If sbrk handed back the bytes immediately following the tail block,
           merge rather than leaving a seam the allocator can never cross. */
        if (tail->free && (char *)(tail + 1) + tail->size == (char *)fresh)
        {
            tail->size += sizeof(block_t) + fresh->size;
            tail->next = NULL;
            return tail;
        }
    }
    return fresh;
}

static void split(block_t *block, size_t size)
{
    /* Only split when the remainder can hold a header plus something useful. */
    if (block->size < size + sizeof(block_t) + ALIGN)
    {
        return;
    }
    block_t *rest = (block_t *)((char *)(block + 1) + size);
    rest->size = block->size - size - sizeof(block_t);
    rest->next = block->next;
    rest->free = 1;
    block->size = size;
    block->next = rest;
}

void *malloc(size_t size)
{
    if (size == 0)
    {
        size = 1;
    }
    size = align_up(size, ALIGN);

    for (block_t *block = heap_head; block != NULL; block = block->next)
    {
        if (block->free && block->size >= size)
        {
            split(block, size);
            block->free = 0;
            return block + 1;
        }
    }

    block_t *fresh = grow(size);
    if (fresh == NULL)
    {
        return NULL;
    }
    split(fresh, size);
    fresh->free = 0;
    return fresh + 1;
}

void free(void *ptr)
{
    if (ptr == NULL)
    {
        return;
    }
    block_t *block = (block_t *)ptr - 1;
    block->free = 1;

    /* Coalesce forward as far as the run of free blocks goes. Backward
       coalescing would need a footer or a back pointer; with this allocation
       pattern it is not worth the bytes. */
    while (block->next != NULL && block->next->free &&
           (char *)(block + 1) + block->size == (char *)block->next)
    {
        block->size += sizeof(block_t) + block->next->size;
        block->next = block->next->next;
    }
}

void *calloc(size_t count, size_t size)
{
    size_t total = count * size;

    if (count != 0 && total / count != size)
    {
        return NULL; /* overflow */
    }
    void *ptr = malloc(total);
    if (ptr != NULL)
    {
        memset(ptr, 0, total);
    }
    return ptr;
}

void *realloc(void *ptr, size_t size)
{
    if (ptr == NULL)
    {
        return malloc(size);
    }
    if (size == 0)
    {
        free(ptr);
        return NULL;
    }

    block_t *block = (block_t *)ptr - 1;
    if (block->size >= size)
    {
        return ptr;
    }

    void *fresh = malloc(size);
    if (fresh == NULL)
    {
        return NULL;
    }
    memcpy(fresh, ptr, block->size);
    free(ptr);
    return fresh;
}
