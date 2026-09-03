#ifndef BLUETOOTH_AUDIO_BRIDGE_H
#define BLUETOOTH_AUDIO_BRIDGE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct bab_engine bab_engine;

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
    uint8_t routing_enabled;
    uint8_t policy_ready;
    uint32_t inputs_detected;
    uint32_t inputs_routed;
    char default_output_name[512];
    char last_error[512];
} bab_status;

typedef struct bab_route_status {
    char input_name[512];
    char input_address[64];
    char output_name[512];
    uint8_t ready;
    char codec[64];
    uint32_t sample_rate;
    uint32_t channels;
    char last_error[512];
} bab_route_status;

bab_engine *bab_engine_create(char *error, size_t error_size);
int bab_engine_set_levels(bab_engine *engine, const bab_levels *levels, char *error, size_t error_size);
int bab_engine_set_enabled(bab_engine *engine, uint8_t enabled, char *error, size_t error_size);
int bab_engine_tick(bab_engine *engine, char *error, size_t error_size);
void bab_engine_status(const bab_engine *engine, bab_status *status);
uint32_t bab_engine_route_count(const bab_engine *engine);
int bab_engine_route_status(const bab_engine *engine, uint32_t index, bab_route_status *status);
void bab_engine_destroy(bab_engine *engine);

#ifdef __cplusplus
}
#endif

#endif
