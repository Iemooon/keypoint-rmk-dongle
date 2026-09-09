MEMORY
{
  /* NOTE 1 K = 1 KiB = 1024 bytes */
  /* The official Nordic nRF52840 Dongle (PCA10059) ships with the Open
     Bootloader; the application partition starts at 0x10000 (matches the
     Zephyr code-partition used by the ZMK receiver that worked on this
     dongle) and runs to the bootloader at 0xE0000. */
  FLASH : ORIGIN = 0x00010000, LENGTH = 832K
  RAM : ORIGIN = 0x20000008, LENGTH = 255K
}
