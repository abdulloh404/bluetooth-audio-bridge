#ifndef BLUETOOTH_AUDIO_BRIDGE_H
#define BLUETOOTH_AUDIO_BRIDGE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct bab_engine bab_engine;

typedef struct bab_config {
    const char *iphone_address;
    const char *headphones_address;
} bab_config;

typedef struct bab_levels {
    float phone_gain;
    float desktop_gain;
    float master_gain;
    uint8_t phone_mute;
    uint8_t desktop_mute;
    uint8_t master_mute;
} bab_levels;

typedef struct bab_status {
    uint8_t pipewire_connected;
    uint8_t route_ready;
    uint8_t phone_policy_ready;
    uint8_t phone_ready;
    uint8_t headphones_ready;
    uint8_t routing_enabled;
    uint32_t sample_rate;
    uint32_t channels;
    char codec[64];
    char phone_stream_state[64];
    char output_stream_state[64];
    char last_error[512];
} bab_status;

bab_engine *bab_engine_create(const bab_config *config, char *error, size_t error_size);
int bab_engine_set_levels(bab_engine *engine, const bab_levels *levels, char *error, size_t error_size);
int bab_engine_set_enabled(bab_engine *engine, uint8_t enabled, char *error, size_t error_size);
int bab_engine_tick(bab_engine *engine, char *error, size_t error_size);
void bab_engine_status(const bab_engine *engine, bab_status *status);
void bab_engine_destroy(bab_engine *engine);

#ifdef __cplusplus
}
#endif

#endif
