/* Minimal bare-metal entry for the p2w native backend on RP2350 (Cortex-M33).
 *
 * Provides the Cortex-M vector table, a reset handler that initialises .data/.bss
 * and calls the compiled program's `main`, and `p2w_putc` (the runtime's only
 * host import) writing bytes to UART0. This is the device counterpart of the
 * host harness's putc.c.
 *
 * Hardware caveat: this links a complete Cortex-M image, but actually *running*
 * it on a Pico 2 still needs clock/UART pin setup before UART0 will emit — that
 * part is scoped in PICO_BACKEND.md and still requires a board to validate.
 * Until then UART0 is poked directly (works once clocks are configured). The
 * OTHER hardware requirement, the RP2350 bootrom IMAGE_DEF metadata block, is
 * below (`image_def[]`) — the bootrom refuses to launch a flash image without
 * one, so this is not optional.
 */

#include <stdint.h>

extern int main(void);

/* Linker-script symbols for .data/.bss init. */
extern uint32_t __data_start, __data_end, __data_load, __bss_start, __bss_end;
extern uint32_t __stack_top;

/* RP2350 UART0: data register at the peripheral base (write a byte to TX). */
#define UART0_DR (*(volatile uint32_t *)0x40070000)

void p2w_putc(unsigned char c) {
    UART0_DR = c;
}

/* Byte source for input(). UART RX needs the flag-register wait (and clock
 * setup) that's part of the hardware-gated bring-up; until then input() on
 * the device sees immediate end-of-input and returns "". */
int p2w_getc(void) {
    return -1;
}

void Reset_Handler(void) {
    /* Copy initialised data from flash to RAM. */
    uint32_t *src = &__data_load;
    for (uint32_t *dst = &__data_start; dst < &__data_end;) {
        *dst++ = *src++;
    }
    /* Zero BSS. */
    for (uint32_t *p = &__bss_start; p < &__bss_end;) {
        *p++ = 0;
    }
    main();
    for (;;) {
        /* halt */
    }
}

void Default_Handler(void) {
    for (;;) {
    }
}

/* The Cortex-M vector table: initial SP, then exception handlers. The first two
 * entries (SP + reset) are what the core fetches on boot. */
__attribute__((section(".vectors"), used))
void (*const vector_table[])(void) = {
    (void (*)(void))(&__stack_top), /* initial stack pointer */
    Reset_Handler,                  /* reset */
    Default_Handler,                /* NMI */
    Default_Handler,                /* HardFault */
};

/* The RP2350 bootrom scans the first 4 KB of flash for a block beginning with
 * PICOBIN_BLOCK_MARKER_START; without one it treats the chip as unprogrammed
 * and never launches this image (RP2350 datasheet, section 5.9.5.1, "Minimum
 * Arm IMAGE_DEF" — verified against the primary source, not a secondhand
 * writeup, since getting these bytes wrong just means "won't boot" but isn't
 * worth guessing at). This exact 20-byte / 5-word sequence is the minimum
 * valid block: a single ITEM_1BS_IMAGE_TYPE item (EXE, secure, Arm, RP2350)
 * followed by the mandatory ITEM_2BS_LAST terminator, in a block loop that
 * points to itself (the only block). It names no entry point, so the bootrom
 * falls back to the Cortex-M vector table above — nothing about Reset_Handler
 * or the vector table changes because of this. */
__attribute__((section(".image_def"), used))
const uint32_t image_def[] = {
    0xffffded3, /* PICOBIN_BLOCK_MARKER_START */
    0x10210142, /* ITEM_1BS_IMAGE_TYPE: EXE | SECURE | Arm | RP2350 chip */
    0x000001ff, /* ITEM_2BS_LAST */
    0x00000000, /* next block in the loop: 0 = link to self */
    0xab123579, /* PICOBIN_BLOCK_MARKER_END */
};
