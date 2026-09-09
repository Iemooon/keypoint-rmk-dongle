MEMORY
{
  /* NOTE 1 K = 1 KiB = 1024 bytes */
  /* The official Nordic nRF52840 Dongle (PCA10059) boots its application at
     0x1000, straight after the MBR - byte-level proof: the working ZMK
     receiver hex (firmware_receiver_nrf52840dongle.hex) is one continuous
     image linked at 0x00001000. The old 0x10000 base (copied from Zephyr
     fstab-stock slot0) left every RMK image stranded where nothing jumps.
     Up to 0xE0000 the Nordic USB bootloader still owns the top. */
  FLASH : ORIGIN = 0x00001000, LENGTH = 892K
  RAM : ORIGIN = 0x20000008, LENGTH = 255K
}
