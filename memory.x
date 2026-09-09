MEMORY
{
  /* NOTE 1 K = 1 KiB = 1024 bytes */
  /* KeyPoint halves: nRF52840 boards with an Adafruit-style UF2 bootloader,
     application starts at 0x1000. This is the stock example memory.x. */
  FLASH : ORIGIN = 0x00001000, LENGTH = 1020K
  RAM : ORIGIN = 0x20000008, LENGTH = 255K

  /* These values correspond to the nRF52840 */
  /* FLASH : ORIGIN = 0x00000000, LENGTH = 1024K */
  /* RAM : ORIGIN = 0x20000000, LENGTH = 256K */
}
