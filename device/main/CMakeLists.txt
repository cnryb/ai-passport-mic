idf_component_register(
    SRCS "app_main.c"
         "protocol.c"
         "power_policy.c"
         "audio.c"
         "battery.c"
         "link.c"
         "buttons.c"
         "screen.c"
         "screen_render.c"
         "screen_state.c"
    INCLUDE_DIRS "."
    REQUIRES driver
             esp_driver_i2s
             esp_driver_i2c
             esp_driver_gpio
             esp_driver_spi
             esp_driver_usb_serial_jtag
             esp_adc
             esp_timer
             esp_pm
             esp_lcd
             bt
             nvs_flash
             esp_codec_dev
)
