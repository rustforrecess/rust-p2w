/* Minimal RP2350 (Cortex-M33) link script for the p2w native backend.
 *
 * Memory map: external QSPI flash is execute-in-place at 0x10000000 (the Pico 2
 * has 4 MB); SRAM is 520 KB at 0x20000000. This places the vector table at the
 * start of flash and the stack at the top of SRAM. NOTE: booting on real
 * hardware additionally needs the RP2350 bootrom IMAGE_DEF metadata block — see
 * PICO_BACKEND.md; this script is enough to link a complete Cortex-M image and
 * verify the toolchain, not yet to guarantee the bootrom runs it. */

ENTRY(Reset_Handler)

MEMORY
{
    FLASH (rx)  : ORIGIN = 0x10000000, LENGTH = 4M
    RAM   (rwx) : ORIGIN = 0x20000000, LENGTH = 520K
}

__stack_top = ORIGIN(RAM) + LENGTH(RAM);

SECTIONS
{
    .vectors : { KEEP(*(.vectors)) } > FLASH
    .text    : { *(.text*) *(.rodata*) } > FLASH

    /* Initialised data: stored in flash, copied to RAM by Reset_Handler. */
    .data : AT (ADDR(.text) + SIZEOF(.text))
    {
        __data_start = .;
        *(.data*)
        __data_end = .;
    } > RAM
    __data_load = LOADADDR(.data);

    .bss (NOLOAD) :
    {
        __bss_start = .;
        *(.bss*)
        *(COMMON)
        __bss_end = .;
    } > RAM
}
