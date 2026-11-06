/* Minimal bare-metal entry for the p2w native backend on RP2350 (Cortex-M33).
 *
 * Provides the Cortex-M vector table, a reset handler that initialises .data/.bss
 * and calls the compiled program's `main`, and `p2w_putc` (the runtime's only
 * host import) writing bytes to UART0. This is the device counterpart of the
 * host harness's putc.c.
 *
 * Hardware bring-up: `clocks_and_uart_init` (below) brings up the 12 MHz
 * crystal, releases reset on UART0/IO_BANK0/PADS_BANK0, routes UART0 onto
 * GPIO0/GPIO1 and programs the baud rate, so UART0 actually emits on real
 * hardware. The RP2350 bootrom IMAGE_DEF metadata block, below
 * (`image_def[]`), is the other hardware requirement — the bootrom refuses to
 * launch a flash image without one, so it is not optional either.
 */

#include <stdint.h>

extern int main(void);

/* Linker-script symbols for .data/.bss init. */
extern uint32_t __data_start, __data_end, __data_load, __bss_start, __bss_end;
extern uint32_t __stack_top;

/* Every peripheral register below is a plain memory-mapped 32-bit register;
 * offsets and bit fields are taken from the RP2350 datasheet (read for the
 * facts, this code and its comments are original) and cross-checked against
 * the pico-sdk register structs the datasheet itself quotes as illustration.
 * Section 2.1.3 ("Atomic Register Access") documents the +0x2000/+0x3000
 * address aliases used here: a write there sets, respectively clears, only
 * the written 1-bits in the real register, without a read-modify-write.
 */

/* RESETS (datasheet 7.5): most peripherals, including UART0, IO_BANK0 and
 * PADS_BANK0, are held in reset at power-up (RESET register resets to
 * all-1s) and must be explicitly released before they respond at all. */
#define RESETS_BASE          0x40020000u
#define RESETS_RESET_CLR     (*(volatile uint32_t *)(RESETS_BASE + 0x3000))
#define RESETS_RESET_DONE    (*(volatile uint32_t *)(RESETS_BASE + 0x8))
#define RESETS_UART0_BIT      (1u << 26)
#define RESETS_PADS_BANK0_BIT (1u << 9)
#define RESETS_IO_BANK0_BIT   (1u << 6)

/* XOSC, the 12 MHz crystal (datasheet 8.2.8). ROSC, what the chip boots from,
 * is explicitly documented (8.1) as too imprecise for "frequency sensitive
 * interfaces such as USB or UART" — this is why XOSC bring-up is needed at
 * all, not just a clock-speed nice-to-have. */
#define XOSC_BASE                      0x40048000u
#define XOSC_CTRL                      (*(volatile uint32_t *)(XOSC_BASE + 0x00))
#define XOSC_STATUS                    (*(volatile uint32_t *)(XOSC_BASE + 0x04))
#define XOSC_STARTUP                   (*(volatile uint32_t *)(XOSC_BASE + 0x0c))
#define XOSC_CTRL_FREQ_RANGE_1_15MHZ   0xaa0u
#define XOSC_CTRL_ENABLE_VALUE_ENABLE  0xfabu
#define XOSC_CTRL_ENABLE_LSB           12
#define XOSC_STATUS_STABLE_BIT         (1u << 31)
/* In units of 256 crystal periods, and undefined out of reset, so it has to
 * be written: 47 units is 1 ms at 12 MHz, times the SDK's margin of 6. */
#define XOSC_STARTUP_DELAY             (47u * 6u)

/* CLOCKS (datasheet 8.1.6). clk_sys already defaults to tracking clk_ref
 * (CLK_SYS_CTRL.SRC resets to clk_ref), so switching clk_ref onto XOSC moves
 * the whole system clock onto the crystal without touching CLK_SYS_CTRL.
 * clk_peri, what feeds UART0, defaults to tracking clk_sys with no divider
 * once its own enable bit is set — it is disabled out of reset. The full PLL
 * (150 MHz) is a separate, later milestone: a precise 12 MHz is already
 * enough to hit standard baud rates within UART tolerance. */
#define CLOCKS_BASE                 0x40010000u
#define CLK_REF_CTRL                (*(volatile uint32_t *)(CLOCKS_BASE + 0x30))
#define CLK_REF_SELECTED            (*(volatile uint32_t *)(CLOCKS_BASE + 0x38))
#define CLK_PERI_CTRL               (*(volatile uint32_t *)(CLOCKS_BASE + 0x48))
#define CLK_REF_CTRL_SRC_XOSC       0x2u
#define CLK_REF_SELECTED_XOSC_BIT   (1u << 2)
#define CLK_PERI_CTRL_ENABLE_BIT    (1u << 11)

/* IO_BANK0 (datasheet 9.11.1): FUNCSEL picks which peripheral drives a GPIO.
 * GPIO0/GPIO1 are the standard Pico UART0 TX/RX pins; FUNCSEL 0x02 on each
 * selects uart0_tx and uart0_rx respectively (verified against the GPIO0_CTRL
 * and GPIO1_CTRL enumerated-value lists, not the summary pin table, since the
 * summary table's column labels don't equal the numeric FUNCSEL value). */
#define IO_BANK0_BASE            0x40028000u
#define GPIO0_CTRL               (*(volatile uint32_t *)(IO_BANK0_BASE + 0x004))
#define GPIO1_CTRL               (*(volatile uint32_t *)(IO_BANK0_BASE + 0x00c))
#define GPIO_FUNCSEL_UART0_TX    0x02u
#define GPIO_FUNCSEL_UART0_RX    0x02u

/* PADS_BANK0 (datasheet 9.11.3). Unlike RP2040, every RP2350 pad resets with
 * its isolation latch set and its input buffer disabled (9.3) — software
 * must clear ISO and set IE before a pad does anything, output pins
 * included. This value keeps every other field (DRIVE, PDE, SCHMITT) at its
 * reset default and only changes ISO (bit 8, cleared) and IE (bit 6, set). */
#define PADS_BANK0_BASE          0x40038000u
#define PADS_GPIO0               (*(volatile uint32_t *)(PADS_BANK0_BASE + 0x04))
#define PADS_GPIO1               (*(volatile uint32_t *)(PADS_BANK0_BASE + 0x08))
#define PADS_GPIO_IE_NO_ISO      0x56u

/* UART0 (PL011, datasheet 12.1). */
#define UART0_BASE   0x40070000u
#define UART0_DR     (*(volatile uint32_t *)(UART0_BASE + 0x000))
#define UART0_FR     (*(volatile uint32_t *)(UART0_BASE + 0x018))
#define UART0_IBRD   (*(volatile uint32_t *)(UART0_BASE + 0x024))
#define UART0_FBRD   (*(volatile uint32_t *)(UART0_BASE + 0x028))
#define UART0_LCR_H  (*(volatile uint32_t *)(UART0_BASE + 0x02c))
#define UART0_CR     (*(volatile uint32_t *)(UART0_BASE + 0x030))
#define UART0_FR_TXFF (1u << 5)

static void clocks_and_uart_init(void) {
    /* Crystal first: clk_sys follows clk_ref, so moving clk_ref onto a crystal
     * that isn't oscillating yet would stop the core. */
    XOSC_CTRL = XOSC_CTRL_FREQ_RANGE_1_15MHZ;
    XOSC_STARTUP = XOSC_STARTUP_DELAY;
    XOSC_CTRL = XOSC_CTRL_FREQ_RANGE_1_15MHZ
              | (XOSC_CTRL_ENABLE_VALUE_ENABLE << XOSC_CTRL_ENABLE_LSB);
    while (!(XOSC_STATUS & XOSC_STATUS_STABLE_BIT)) {
        /* wait */
    }

    CLK_REF_CTRL = CLK_REF_CTRL_SRC_XOSC;
    while (!(CLK_REF_SELECTED & CLK_REF_SELECTED_XOSC_BIT)) {
        /* wait */
    }

    /* Turn on clk_peri (tracks clk_sys, which tracks clk_ref, now XOSC). This
     * comes before releasing UART0 from reset: the UART runs from clk_peri, so
     * the SDK brings up clocks first and only then unresets the blocks. */
    CLK_PERI_CTRL = CLK_PERI_CTRL_ENABLE_BIT;

    /* Release reset on UART0, IO_BANK0 and PADS_BANK0. */
    const uint32_t blocks = RESETS_UART0_BIT | RESETS_PADS_BANK0_BIT | RESETS_IO_BANK0_BIT;
    RESETS_RESET_CLR = blocks;
    while ((RESETS_RESET_DONE & blocks) != blocks) {
        /* wait */
    }

    /* Wire GPIO0/GPIO1 to UART0 TX/RX. */
    PADS_GPIO0 = PADS_GPIO_IE_NO_ISO;
    PADS_GPIO1 = PADS_GPIO_IE_NO_ISO;
    GPIO0_CTRL = GPIO_FUNCSEL_UART0_TX;
    GPIO1_CTRL = GPIO_FUNCSEL_UART0_RX;

    /* 115200 8N1 off a precise 12 MHz clk_peri (datasheet 12.1.3.2.1):
     * divisor = clk / (16 * baud) = 12000000 / 1843200 = 6.5104..., so
     * IBRD = 6 and FBRD = round(0.5104 * 64) = 33. */
    UART0_IBRD = 6;
    UART0_FBRD = 33;
    UART0_LCR_H = (1u << 4) | (0x3u << 5); /* FEN (FIFOs on), WLEN = 8 bits */
    UART0_CR = (1u << 0) | (1u << 8) | (1u << 9); /* UARTEN, TXE, RXE */
}

void p2w_putc(unsigned char c) {
    while (UART0_FR & UART0_FR_TXFF) {
        /* wait for room in the TX FIFO */
    }
    UART0_DR = c;
}

/* Byte source for input(). Clock/pin bring-up above makes UART0 RX live, but
 * the flag-register polling read itself isn't implemented yet, so input() on
 * the device still sees immediate end-of-input and returns "". */
int p2w_getc(void) {
    return -1;
}

/* The runtime's generic release path references this for every program. No
 * file can be open on a board: open() itself has no definition here, so a
 * program that calls it fails at link time. */
void p2w_host_file_close(int handle) {
    (void)handle;
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
    clocks_and_uart_init();
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
