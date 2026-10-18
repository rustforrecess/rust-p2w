/* Minimal RP2350 (Cortex-M33) link script for the p2w native backend.
 *
 * Memory map: external QSPI flash is execute-in-place at 0x10000000 (the Pico 2
 * has 4 MB); SRAM is 520 KB at 0x20000000. This places the vector table at the
 * start of flash, the bootrom IMAGE_DEF block (device/boot.c's `image_def[]`)
 * right after it, and the stack at the top of SRAM. Both .vectors and
 * .image_def together are under 40 bytes, comfortably inside the "first 4 kB
 * of flash" the bootrom actually scans (RP2350 datasheet 5.9.5), so no
 * explicit placement address is needed beyond section ORDER. */

ENTRY(Reset_Handler)

MEMORY
{
    FLASH (rx)  : ORIGIN = 0x10000000, LENGTH = 4M
    RAM   (rwx) : ORIGIN = 0x20000000, LENGTH = 520K
}

__stack_top = ORIGIN(RAM) + LENGTH(RAM);

SECTIONS
{
    .vectors    : { KEEP(*(.vectors)) } > FLASH
    .image_def  : { KEEP(*(.image_def)) } > FLASH
    .text       : { *(.text*) *(.rodata*) } > FLASH

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
