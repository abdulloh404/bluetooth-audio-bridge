#include "bluetooth_audio_bridge.h"

#include <pipewire/pipewire.h>
#include <spa/param/audio/format-utils.h>
#include <spa/param/props.h>
#include <spa/pod/builder.h>

#include <algorithm>
#include <cerrno>
#include <cctype>
#include <cmath>
#include <cstring>
#include <map>
#include <memory>
#include <mutex>
#include <set>
#include <stdexcept>
#include <string>
#include <utility>
#include <vector>

namespace {

using Properties = std::map<std::string, std::string>;
using Route = std::pair<uint32_t, uint32_t>;
constexpr uint32_t invalid_id = PW_ID_ANY;

void copy_text(char *destination, size_t size, const char *source) noexcept {
    if (!destination || size == 0) return;
    if (!source) source = "";
    const auto count = std::min(size - 1, std::strlen(source));
    std::memcpy(destination, source, count);
    destination[count] = '\0';
}

const std::string &property(const Properties &properties, const char *key) {
    static const std::string empty;
    const auto it = properties.find(key);
    return it == properties.end() ? empty : it->second;
}

void merge(Properties &properties, const spa_dict *update) {
    if (!update) return;
    const spa_dict_item *item;
    spa_dict_for_each(item, update) {
        if (item->value) properties[item->key] = item->value;
        else properties.erase(item->key);
    }
}

uint32_t number(const std::string &value) noexcept {
    if (value.empty()) return invalid_id;
    uint64_t result = 0;
    for (const auto c : value) {
        if (c < '0' || c > '9') return invalid_id;
        result = result * 10 + static_cast<unsigned>(c - '0');
        if (result >= invalid_id) return invalid_id;
    }
    return static_cast<uint32_t>(result);
}

std::string address(const std::string &value) {
    std::string result;
    for (const auto c : value) {
        if (c == ':' || c == '_' || c == '-') continue;
        if (!std::isxdigit(static_cast<unsigned char>(c))) return {};
        result += static_cast<char>(std::toupper(static_cast<unsigned char>(c)));
    }
    return result.size() == 12 ? result : std::string{};
}

bool matches_address(const Properties &properties, const std::string &expected) {
    for (const auto *key : {"api.bluez5.address", "device.string", "api.bluez5.path", "device.name"}) {
        auto value = property(properties, key);
        const auto path = value.find("/dev_");
        if (path != std::string::npos) value = value.substr(path + 5);
        if (value.compare(0, 11, "bluez_card.") == 0) value = value.substr(11);
        if (address(value) == expected) return true;
    }
    return false;
}

bool same_levels(const std::vector<float> &left, const std::vector<float> &right) {
    return left.size() == right.size() && std::equal(left.begin(), left.end(), right.begin(),
        [](float a, float b) { return std::fabs(a - b) <= 0.00001f; });
}

std::vector<float> volume_array(const spa_pod *param, uint32_t key) {
    const auto *prop = spa_pod_find_prop(param, nullptr, key);
    if (!prop || !spa_pod_is_array(&prop->value) || SPA_POD_ARRAY_VALUE_TYPE(&prop->value) != SPA_TYPE_Float ||
        SPA_POD_ARRAY_VALUE_SIZE(&prop->value) != sizeof(float)) return {};
    const auto count = SPA_POD_ARRAY_N_VALUES(&prop->value);
    if (count == 0 || count > SPA_AUDIO_MAX_CHANNELS) return {};
    const auto *values = static_cast<const float *>(SPA_POD_ARRAY_VALUES(&prop->value));
    std::vector<float> result(values, values + count);
    if (std::any_of(result.begin(), result.end(), [](float value) { return !std::isfinite(value) || value < 0.0f; })) return {};
    return result;
}

struct Engine;
enum class Kind { Node, Port, Device, Link };

struct Object {
    Engine *engine;
    uint32_t id;
    uint32_t permissions;
    Kind kind;
    Properties props;
    pw_proxy *proxy = nullptr;
    spa_hook listener{};
    bool listening = false;
    uint32_t subscriptions = 0;
    bool props_readable = false;
    bool format_readable = false;
    bool props_writable = false;
    pw_node_state node_state = PW_NODE_STATE_CREATING;
    pw_link_state link_state = PW_LINK_STATE_INIT;
    uint32_t rate = 0;
    uint32_t channels = 0;
    std::vector<float> soft_volumes;
    std::vector<float> channel_volumes;
    uint32_t volume_key = 0;
    std::vector<float> original;
    std::vector<float> written;
    bool owns_volume = false;
    bool volume_pending = false;
    bool external_volume = false;
    int volume_sequence = 0;
    std::string volume_error;

    Object(Engine *owner, uint32_t global_id, uint32_t access, Kind type)
        : engine(owner), id(global_id), permissions(access), kind(type) {}
    ~Object() {
        if (listening) spa_hook_remove(&listener);
        if (proxy) pw_proxy_destroy(proxy);
    }
};

struct Owned {
    Engine *engine;
    pw_proxy *proxy = nullptr;
    spa_hook listener{};
    uint32_t id = invalid_id;
    bool failed = false;
    bool listening = false;

    explicit Owned(Engine *owner) : engine(owner) {}
    ~Owned() {
        if (listening) spa_hook_remove(&listener);
        if (proxy) pw_proxy_destroy(proxy);
    }
};

struct LoopLock {
    pw_thread_loop *loop;
    explicit LoopLock(pw_thread_loop *value) : loop(value) { pw_thread_loop_lock(loop); }
    ~LoopLock() { pw_thread_loop_unlock(loop); }
};

struct Engine {
    std::string phone_address;
    std::string headphones_address;
    pw_thread_loop *loop = nullptr;
    pw_context *context = nullptr;
    pw_core *core = nullptr;
    pw_registry *registry = nullptr;
    spa_hook core_listener{};
    spa_hook registry_listener{};
    bool core_listening = false;
    bool registry_listening = false;
    bool started = false;
    bool connected = false;
    bool fatal = false;
    bool enabled = false;
    int sync_sequence = 0;
    int completed_sequence = -1;
    int param_sequence = 100;
    uint32_t phone_id = invalid_id;
    uint32_t headphones_id = invalid_id;
    std::map<uint32_t, std::unique_ptr<Object>> objects;
    std::map<Route, std::unique_ptr<Owned>> links;
    std::set<uint32_t> level_targets;
    bab_levels levels{1.0f, 1.0f, 1.0f, 0, 0, 0};
    bab_status current{};
    char graph_error[512]{};

    explicit Engine(const bab_config &config)
        : phone_address(address(config.iphone_address ? config.iphone_address : "")),
          headphones_address(address(config.headphones_address ? config.headphones_address : "")) {
        if (phone_address.empty() || headphones_address.empty())
            throw std::runtime_error("Two valid Bluetooth addresses are required");
        if (phone_address == headphones_address)
            throw std::runtime_error("The iPhone and headphones must be different devices");
    }

    ~Engine() {
        if (started) {
            // ส่งคำสั่งคืน software volume และลบเฉพาะ links ที่เป็นเจ้าของ ก่อนหยุด PipeWire loop
            {
                LoopLock lock(loop);
                try {
                    flush();
                    for (auto &entry : objects) restore_levels(*entry.second);
                } catch (...) {}
                links.clear();
                try { flush(); } catch (...) {}
            }
            pw_thread_loop_stop(loop);
        }
        links.clear();
        objects.clear();
        if (registry) {
            if (registry_listening) spa_hook_remove(&registry_listener);
            pw_proxy_destroy(reinterpret_cast<pw_proxy *>(registry));
        }
        if (core) {
            if (core_listening) spa_hook_remove(&core_listener);
            pw_core_disconnect(core);
        }
        if (context) pw_context_destroy(context);
        if (loop) pw_thread_loop_destroy(loop);
    }

    static void registry_global(void *, uint32_t, uint32_t, const char *, uint32_t, const spa_dict *) noexcept;
    static void registry_remove(void *, uint32_t) noexcept;
    static void node_info(void *, const pw_node_info *) noexcept;
    static void node_param(void *, int, uint32_t, uint32_t, uint32_t, const spa_pod *) noexcept;
    static void port_info(void *, const pw_port_info *) noexcept;
    static void device_info(void *, const pw_device_info *) noexcept;
    static void link_info(void *, const pw_link_info *) noexcept;
    void init();
    void flush();
    void reconcile();
    uint32_t find_port(uint32_t node, const char *direction, const char *channel) const;
    Object *device_for(const Object &node) const;
    bool matches(const Object &node, const std::string &expected) const;
    std::string codec_for(const Object &node) const;
    void add_link(const Route &route);
    bool link_ready(const Route &route) const;
    bool foreign_phone_link() const;
    bool desktop_target(const Object &node) const;
    void apply_levels(Object &object, float gain, bool phone);
    void restore_levels(Object &object);
    void write_levels(Object &object, const std::vector<float> &values);
    void callback_failure() noexcept {
        fatal = true;
        copy_text(graph_error, sizeof(graph_error), "PipeWire event processing failed");
    }
};

const pw_proxy_events owned_events = [] {
    pw_proxy_events events{};
    events.version = PW_VERSION_PROXY_EVENTS;
    events.bound = [](void *data, uint32_t id) noexcept { static_cast<Owned *>(data)->id = id; };
    events.removed = [](void *data) noexcept { static_cast<Owned *>(data)->failed = true; };
    events.error = [](void *data, int, int, const char *message) noexcept {
        auto &owned = *static_cast<Owned *>(data);
        owned.failed = true;
        copy_text(owned.engine->graph_error, sizeof(owned.engine->graph_error), message);
    };
    return events;
}();

void Engine::flush() {
    if (!core || fatal) return;
    const int sequence = pw_core_sync(core, PW_ID_CORE, ++sync_sequence);
    if (sequence < 0) throw std::runtime_error("Cannot synchronize with PipeWire");
    timespec deadline{};
    pw_thread_loop_get_time(loop, &deadline, SPA_NSEC_PER_SEC);
    while (completed_sequence != sequence && !fatal) {
        if (pw_thread_loop_timed_wait_full(loop, &deadline) < 0)
            throw std::runtime_error("Timed out waiting for PipeWire to confirm audio changes");
    }
}

void Engine::init() {
    static std::once_flag initialized;
    std::call_once(initialized, [] { pw_init(nullptr, nullptr); });
    loop = pw_thread_loop_new("bluetooth-audio-bridge", nullptr);
    if (!loop) throw std::runtime_error("Cannot create the PipeWire thread loop");
    context = pw_context_new(pw_thread_loop_get_loop(loop), nullptr, 0);
    if (!context) throw std::runtime_error("Cannot create the PipeWire context");
    core = pw_context_connect(context, pw_properties_new(PW_KEY_APP_NAME, "Bluetooth Audio Bridge", nullptr), 0);
    if (!core) throw std::runtime_error("Cannot connect to PipeWire; check the user audio session");
    static const pw_core_events core_events = [] {
        pw_core_events events{};
        events.version = PW_VERSION_CORE_EVENTS;
        events.info = [](void *data, const pw_core_info *) noexcept { static_cast<Engine *>(data)->connected = true; };
        events.done = [](void *data, uint32_t id, int sequence) noexcept {
            auto &engine = *static_cast<Engine *>(data);
            if (id == PW_ID_CORE) engine.completed_sequence = sequence;
            pw_thread_loop_signal(engine.loop, false);
        };
        events.error = [](void *data, uint32_t id, int, int result, const char *message) noexcept {
            auto &engine = *static_cast<Engine *>(data);
            if (id == PW_ID_CORE || result == -EPIPE || result == -ECONNRESET) {
                engine.connected = false;
                engine.fatal = true;
            }
            copy_text(engine.graph_error, sizeof(engine.graph_error), message);
            pw_thread_loop_signal(engine.loop, false);
        };
        return events;
    }();
    pw_core_add_listener(core, &core_listener, &core_events, this);
    core_listening = true;
    registry = pw_core_get_registry(core, PW_VERSION_REGISTRY, 0);
    if (!registry) throw std::runtime_error("Cannot discover the PipeWire registry");
    static const pw_registry_events registry_events = [] {
        pw_registry_events events{};
        events.version = PW_VERSION_REGISTRY_EVENTS;
        events.global = registry_global;
        events.global_remove = registry_remove;
        return events;
    }();
    pw_registry_add_listener(registry, &registry_listener, &registry_events, this);
    registry_listening = true;
    if (pw_thread_loop_start(loop) < 0) throw std::runtime_error("Cannot start the PipeWire client loop");
    started = true;
    LoopLock lock(loop);
    // รอทั้ง registry และข้อมูลจาก proxy ก่อนตรวจหา links เดิม เพื่อไม่สร้างเส้นทางซ้ำ
    flush();
    flush();
}

void Engine::registry_global(void *data, uint32_t id, uint32_t permissions, const char *type, uint32_t version, const spa_dict *props) noexcept {
    auto &engine = *static_cast<Engine *>(data);
    try {
        Kind kind;
        uint32_t supported;
        if (std::strcmp(type, PW_TYPE_INTERFACE_Node) == 0) { kind = Kind::Node; supported = PW_VERSION_NODE; }
        else if (std::strcmp(type, PW_TYPE_INTERFACE_Port) == 0) { kind = Kind::Port; supported = PW_VERSION_PORT; }
        else if (std::strcmp(type, PW_TYPE_INTERFACE_Device) == 0) { kind = Kind::Device; supported = PW_VERSION_DEVICE; }
        else if (std::strcmp(type, PW_TYPE_INTERFACE_Link) == 0) { kind = Kind::Link; supported = PW_VERSION_LINK; }
        else return;
        auto object = std::make_unique<Object>(&engine, id, permissions, kind);
        merge(object->props, props);
        object->proxy = reinterpret_cast<pw_proxy *>(pw_registry_bind(engine.registry, id, type, std::min(version, supported), 0));
        if (!object->proxy) return;
        auto *pointer = object.get();
        engine.objects[id] = std::move(object);
        if (kind == Kind::Node) {
            static const pw_node_events events = [] { pw_node_events e{}; e.version = PW_VERSION_NODE_EVENTS; e.info = node_info; e.param = node_param; return e; }();
            pw_node_add_listener(reinterpret_cast<pw_node *>(pointer->proxy), &pointer->listener, &events, pointer);
        } else if (kind == Kind::Port) {
            static const pw_port_events events = [] { pw_port_events e{}; e.version = PW_VERSION_PORT_EVENTS; e.info = port_info; return e; }();
            pw_port_add_listener(reinterpret_cast<pw_port *>(pointer->proxy), &pointer->listener, &events, pointer);
        } else if (kind == Kind::Device) {
            static const pw_device_events events = [] { pw_device_events e{}; e.version = PW_VERSION_DEVICE_EVENTS; e.info = device_info; return e; }();
            pw_device_add_listener(reinterpret_cast<pw_device *>(pointer->proxy), &pointer->listener, &events, pointer);
        } else {
            static const pw_link_events events = [] { pw_link_events e{}; e.version = PW_VERSION_LINK_EVENTS; e.info = link_info; return e; }();
            pw_link_add_listener(reinterpret_cast<pw_link *>(pointer->proxy), &pointer->listener, &events, pointer);
        }
        pointer->listening = true;
    } catch (...) { engine.callback_failure(); }
}

void Engine::registry_remove(void *data, uint32_t id) noexcept {
    auto &engine = *static_cast<Engine *>(data);
    for (auto &entry : engine.links) if (entry.second->id == id) entry.second->failed = true;
    engine.level_targets.erase(id);
    engine.objects.erase(id);
}

void Engine::node_info(void *data, const pw_node_info *info) noexcept {
    auto &object = *static_cast<Object *>(data);
    try {
        if (info->change_mask & PW_NODE_CHANGE_MASK_PROPS) merge(object.props, info->props);
        if (info->change_mask & PW_NODE_CHANGE_MASK_STATE) object.node_state = info->state;
        if (info->change_mask & PW_NODE_CHANGE_MASK_PARAMS) {
            for (uint32_t index = 0; index < info->n_params; ++index) {
                const auto &param = info->params[index];
                if (param.id == SPA_PARAM_Props) {
                    object.props_writable = (param.flags & SPA_PARAM_INFO_WRITE) != 0;
                    object.props_readable = (param.flags & SPA_PARAM_INFO_READ) != 0;
                } else if (param.id == SPA_PARAM_Format) {
                    object.format_readable = (param.flags & SPA_PARAM_INFO_READ) != 0;
                    if (!object.format_readable) { object.rate = 0; object.channels = 0; }
                }
            }
        }
        const uint32_t subscriptions = (object.props_readable ? 1u : 0u) | (object.format_readable ? 2u : 0u);
        if (subscriptions != object.subscriptions) {
            uint32_t params[2], count = 0;
            if (object.props_readable) params[count++] = SPA_PARAM_Props;
            if (object.format_readable) params[count++] = SPA_PARAM_Format;
            if (pw_node_subscribe_params(reinterpret_cast<pw_node *>(object.proxy), params, count) >= 0)
                object.subscriptions = subscriptions;
        }
    } catch (...) { object.engine->callback_failure(); }
}

void Engine::node_param(void *data, int sequence, uint32_t id, uint32_t, uint32_t, const spa_pod *param) noexcept {
    auto &object = *static_cast<Object *>(data);
    try {
        if (id == SPA_PARAM_Format) {
            object.rate = 0;
            object.channels = 0;
            spa_audio_info_raw format{};
            uint32_t media_type = 0, subtype = 0;
            if (param && spa_format_parse(param, &media_type, &subtype) >= 0 && media_type == SPA_MEDIA_TYPE_audio &&
                subtype == SPA_MEDIA_SUBTYPE_raw && spa_format_audio_raw_parse(param, &format) >= 0) {
                object.rate = format.rate;
                object.channels = format.channels;
            }
            return;
        }
        if (id != SPA_PARAM_Props || !param) return;
        object.soft_volumes = volume_array(param, SPA_PROP_softVolumes);
        object.channel_volumes = volume_array(param, SPA_PROP_channelVolumes);
        if (!object.volume_key) return;
        const auto &observed = object.volume_key == SPA_PROP_softVolumes ? object.soft_volumes : object.channel_volumes;
        if (object.volume_pending) {
            if (same_levels(observed, object.written)) {
                object.volume_pending = false;
                object.owns_volume = true;
                object.volume_error.clear();
            } else if (sequence == object.volume_sequence) {
                // บาง node ส่งผลเปลี่ยน Props ภายหลัง; ยังเก็บ pending ไว้เพื่อคืนค่าเมื่อได้รับ confirmation
                object.volume_error = "PipeWire has not confirmed the requested software volume; no further writes will be made until confirmed or retried";
            }
        } else if (object.owns_volume && !same_levels(observed, object.written)) {
            // ยกเลิก ownership เมื่อแอปหรือผู้ใช้เปลี่ยน volume เพื่อไม่เขียนทับค่าของผู้อื่น
            object.owns_volume = false;
            object.external_volume = true;
            object.volume_error = "Software volume changed outside the bridge; respecting that change until the next volume or mute command";
        }
    } catch (...) { object.engine->callback_failure(); }
}

void Engine::port_info(void *data, const pw_port_info *info) noexcept {
    auto &object = *static_cast<Object *>(data);
    try {
        merge(object.props, info->props);
        object.props["port.direction"] = info->direction == PW_DIRECTION_INPUT ? "in" : "out";
    } catch (...) { object.engine->callback_failure(); }
}

void Engine::device_info(void *data, const pw_device_info *info) noexcept {
    auto &object = *static_cast<Object *>(data);
    try { merge(object.props, info->props); } catch (...) { object.engine->callback_failure(); }
}

void Engine::link_info(void *data, const pw_link_info *info) noexcept {
    auto &object = *static_cast<Object *>(data);
    try {
        object.props["link.output.node"] = std::to_string(info->output_node_id);
        object.props["link.input.node"] = std::to_string(info->input_node_id);
        object.props["link.output.port"] = std::to_string(info->output_port_id);
        object.props["link.input.port"] = std::to_string(info->input_port_id);
        if (info->change_mask & PW_LINK_CHANGE_MASK_STATE) object.link_state = info->state;
        if (info->state == PW_LINK_STATE_ERROR) {
            for (auto &entry : object.engine->links) {
                if (entry.second->id == object.id) {
                    entry.second->failed = true;
                    copy_text(object.engine->graph_error, sizeof(object.engine->graph_error), info->error);
                }
            }
        }
    } catch (...) { object.engine->callback_failure(); }
}

Object *Engine::device_for(const Object &node) const {
    const auto it = objects.find(number(property(node.props, "device.id")));
    return it != objects.end() && it->second->kind == Kind::Device ? it->second.get() : nullptr;
}

bool Engine::matches(const Object &node, const std::string &expected) const {
    if (matches_address(node.props, expected)) return true;
    const auto *device = device_for(node);
    return device && matches_address(device->props, expected);
}

std::string Engine::codec_for(const Object &node) const {
    // อ่าน codec ที่ transport รายงาน โดยไม่เปลี่ยน profile หรือ codec ที่ระบบเลือกไว้
    auto codec = property(node.props, "api.bluez5.codec");
    if (codec.empty()) {
        const auto *device = device_for(node);
        if (device) codec = property(device->props, "api.bluez5.codec");
    }
    std::transform(codec.begin(), codec.end(), codec.begin(), [](unsigned char c) { return static_cast<char>(std::tolower(c)); });
    return codec;
}

uint32_t Engine::find_port(uint32_t node, const char *direction, const char *channel) const {
    if (node == invalid_id) return invalid_id;
    uint32_t found = invalid_id;
    for (const auto &entry : objects) {
        const auto &object = *entry.second;
        if (object.kind != Kind::Port || number(property(object.props, "node.id")) != node ||
            property(object.props, "port.direction") != direction || property(object.props, "audio.channel") != channel) continue;
        if (found != invalid_id) return invalid_id;
        found = object.id;
    }
    return found;
}

bool Engine::foreign_phone_link() const {
    if (phone_id == invalid_id) return false;
    for (const auto &entry : objects) {
        const auto &object = *entry.second;
        if (object.kind != Kind::Link || object.link_state < PW_LINK_STATE_INIT ||
            number(property(object.props, "link.output.node")) != phone_id) continue;
        const auto owned = links.find({number(property(object.props, "link.output.port")), number(property(object.props, "link.input.port"))});
        if (owned == links.end() || owned->second->id != object.id) return true;
    }
    return false;
}

bool Engine::desktop_target(const Object &node) const {
    if (node.kind != Kind::Node || node.id == phone_id || property(node.props, "media.class") != "Stream/Output/Audio" ||
        property(node.props, "node.virtual") == "true" || device_for(node) || !property(node.props, "api.bluez5.address").empty()) return false;
    bool connected_to_headphones = false;
    for (const auto &entry : objects) {
        const auto &link = *entry.second;
        if (link.kind != Kind::Link || link.link_state < PW_LINK_STATE_PAUSED ||
            number(property(link.props, "link.output.node")) != node.id) continue;
        if (number(property(link.props, "link.input.node")) != headphones_id) return false;
        connected_to_headphones = true;
    }
    return connected_to_headphones;
}

void Engine::add_link(const Route &route) {
    const auto output = objects.find(route.first);
    const auto input = objects.find(route.second);
    if (output == objects.end() || input == objects.end()) return;
    const auto output_node = property(output->second->props, "node.id");
    const auto input_node = property(input->second->props, "node.id");
    const auto output_port = std::to_string(route.first);
    const auto input_port = std::to_string(route.second);
    auto link = std::make_unique<Owned>(this);
    auto *props = pw_properties_new(
        PW_KEY_LINK_OUTPUT_NODE, output_node.c_str(), PW_KEY_LINK_OUTPUT_PORT, output_port.c_str(),
        PW_KEY_LINK_INPUT_NODE, input_node.c_str(), PW_KEY_LINK_INPUT_PORT, input_port.c_str(),
        PW_KEY_LINK_PASSIVE, "false", "object.linger", "false", nullptr);
    if (!props) throw std::bad_alloc();
    link->proxy = reinterpret_cast<pw_proxy *>(pw_core_create_object(core, "link-factory", PW_TYPE_INTERFACE_Link, PW_VERSION_LINK, &props->dict, 0));
    pw_properties_free(props);
    if (!link->proxy) throw std::runtime_error("Cannot create a direct iPhone audio link");
    pw_proxy_add_listener(link->proxy, &link->listener, &owned_events, link.get());
    link->listening = true;
    links.emplace(route, std::move(link));
}

bool Engine::link_ready(const Route &route) const {
    const auto owned = links.find(route);
    if (owned == links.end() || owned->second->failed) return false;
    const auto object = objects.find(owned->second->id);
    return object != objects.end() && object->second->kind == Kind::Link && object->second->link_state >= PW_LINK_STATE_PAUSED;
}

void Engine::write_levels(Object &object, const std::vector<float> &values) {
    uint8_t buffer[2048];
    spa_pod_builder builder = SPA_POD_BUILDER_INIT(buffer, sizeof(buffer));
    const auto *param = static_cast<const spa_pod *>(spa_pod_builder_add_object(&builder, SPA_TYPE_OBJECT_Props, SPA_PARAM_Props,
        object.volume_key, SPA_POD_Array(sizeof(float), SPA_TYPE_Float, static_cast<uint32_t>(values.size()), values.data())));
    if (!param || pw_node_set_param(reinterpret_cast<pw_node *>(object.proxy), SPA_PARAM_Props, 0, param) < 0)
        throw std::runtime_error("PipeWire rejected a software volume change");
    object.written = values;
    object.volume_pending = true;
    object.volume_sequence = ++param_sequence;
    if (pw_node_enum_params(reinterpret_cast<pw_node *>(object.proxy), object.volume_sequence, SPA_PARAM_Props, 0, 1, nullptr) < 0) {
        object.volume_error = "Cannot verify the requested software volume";
        throw std::runtime_error(object.volume_error);
    }
}

void Engine::apply_levels(Object &object, float gain, bool phone) {
    if (object.volume_pending || object.external_volume) return;
    if (!object.owns_volume) {
        if (gain == 1.0f) {
            object.volume_error.clear();
            return;
        }
        // Bluetooth ใช้เฉพาะ softVolumes เพื่อไม่ส่งคำสั่ง volume ไปยัง hardware
        object.volume_key = !object.soft_volumes.empty() ? SPA_PROP_softVolumes : (!phone && !object.channel_volumes.empty() ? SPA_PROP_channelVolumes : 0);
        if (!object.volume_key || !object.props_writable || !(object.permissions & PW_PERM_W)) {
            object.volume_error = "The selected stream does not expose writable software volume; native audio is unchanged";
            return;
        }
        object.original = object.volume_key == SPA_PROP_softVolumes ? object.soft_volumes : object.channel_volumes;
        object.written = object.original;
    }
    std::vector<float> desired = object.original;
    for (auto &value : desired) value *= gain;
    const auto &observed = object.volume_key == SPA_PROP_softVolumes ? object.soft_volumes : object.channel_volumes;
    if (same_levels(desired, observed)) return;
    // คำนวณจาก snapshot เดิมทุกครั้ง ไม่คูณซ้ำจาก param event ที่สะท้อนค่าที่เพิ่งเขียน
    try { write_levels(object, desired); }
    catch (const std::exception &exception) {
        object.volume_error = exception.what();
        if (!object.volume_pending) object.external_volume = true;
    }
}

void Engine::restore_levels(Object &object) {
    if (!object.volume_key) return;
    const auto &observed = object.volume_key == SPA_PROP_softVolumes ? object.soft_volumes : object.channel_volumes;
    if ((object.owns_volume || object.volume_pending) && same_levels(observed, object.written) && !same_levels(observed, object.original)) {
        const auto original = object.original;
        write_levels(object, original);
    }
    object.owns_volume = false;
    object.volume_pending = false;
    object.external_volume = false;
    object.volume_key = 0;
    object.original.clear();
    object.written.clear();
    object.volume_error.clear();
}

void Engine::reconcile() {
    if (fatal) throw std::runtime_error(graph_error[0] ? graph_error : "The PipeWire connection is unavailable");
    current = {};
    current.pipewire_connected = connected;
    current.routing_enabled = enabled;
    copy_text(current.phone_stream_state, sizeof(current.phone_stream_state), "disconnected");
    copy_text(current.output_stream_state, sizeof(current.output_stream_state), "disconnected");
    copy_text(current.codec, sizeof(current.codec), "unknown");
    Object *phone = nullptr;
    Object *headphones = nullptr;
    bool multiple_phones = false, multiple_headphones = false;
    for (auto &entry : objects) {
        auto &object = *entry.second;
        if (object.kind != Kind::Node) continue;
        const auto &media_class = property(object.props, "media.class");
        if ((media_class == "Stream/Output/Audio" || media_class == "Audio/Source" || media_class == "Audio/Source/Virtual") && matches(object, phone_address)) {
            if (phone) multiple_phones = true;
            else phone = &object;
        }
        if (media_class == "Audio/Sink" && matches(object, headphones_address)) {
            if (headphones) multiple_headphones = true;
            else headphones = &object;
        }
    }
    phone_id = phone ? phone->id : invalid_id;
    headphones_id = headphones ? headphones->id : invalid_id;
    std::string error;
    if (!connected) error = "Waiting for the PipeWire connection";
    else if (multiple_phones || multiple_headphones) error = "More than one audio node matches a selected device; refusing ambiguous routing";
    else if (!phone) error = "Waiting for the selected iPhone playback stream";
    if (phone) {
        copy_text(current.phone_stream_state, sizeof(current.phone_stream_state), pw_node_state_as_string(phone->node_state));
        current.phone_policy_ready = !multiple_phones && property(phone->props, "bluetooth-audio-bridge.mode") == "direct" &&
            property(phone->props, "bluetooth-audio-bridge.phone") == "true" && property(phone->props, "node.autoconnect") == "false" &&
            property(phone->props, "node.dont-fallback") == "true" && property(phone->props, "node.dont-reconnect") == "true";
        current.phone_ready = !multiple_phones && phone->node_state != PW_NODE_STATE_ERROR &&
            find_port(phone_id, "out", "FL") != invalid_id && find_port(phone_id, "out", "FR") != invalid_id;
        if (error.empty() && !current.phone_policy_ready)
            error = "Install the direct phone policy, log out and back in, then reconnect the iPhone. Existing playback is left untouched until that policy is loaded.";
        else if (error.empty() && !current.phone_ready) error = "Waiting for usable selected iPhone stereo output ports";
    }
    if (headphones) {
        copy_text(current.output_stream_state, sizeof(current.output_stream_state), pw_node_state_as_string(headphones->node_state));
        const auto codec = codec_for(*headphones);
        copy_text(current.codec, sizeof(current.codec), codec.empty() ? "unknown" : codec.c_str());
        current.sample_rate = headphones->rate;
        current.channels = headphones->channels;
        current.headphones_ready = !multiple_headphones && headphones->node_state != PW_NODE_STATE_ERROR &&
            find_port(headphones_id, "in", "FL") != invalid_id && find_port(headphones_id, "in", "FR") != invalid_id;
    }
    if (error.empty() && !current.headphones_ready) error = "Waiting for usable selected headphone stereo input ports";
    const bool foreign_route = foreign_phone_link();
    if (error.empty() && foreign_route) error = "The selected iPhone already has audio links owned by another client. Reconnect it after loading the direct policy; existing links are left untouched.";
    const bool manage = connected && enabled && current.phone_policy_ready && current.phone_ready && current.headphones_ready && !foreign_route;
    std::set<Route> desired;
    if (manage) {
        for (const auto *channel : {"FL", "FR"}) desired.emplace(find_port(phone_id, "out", channel), find_port(headphones_id, "in", channel));
    }
    for (auto it = links.begin(); it != links.end();) {
        if (!desired.count(it->first) || it->second->failed) it = links.erase(it);
        else ++it;
    }
    for (const auto &route : desired) if (!links.count(route)) add_link(route);
    current.route_ready = desired.size() == 2 && std::all_of(desired.begin(), desired.end(), [this](const Route &route) { return link_ready(route); });
    level_targets.clear();
    if (manage) level_targets.insert(phone_id);
    // คง desktop control เมื่อโทรศัพท์หลุด โดยไม่เปลี่ยนเส้นทางที่แอปเลือกไว้
    if (connected && enabled && current.headphones_ready) {
        for (const auto &entry : objects) if (desktop_target(*entry.second)) level_targets.insert(entry.first);
    }
    for (auto &entry : objects) {
        auto &object = *entry.second;
        if (!level_targets.count(object.id)) restore_levels(object);
        else {
            const bool is_phone = object.id == phone_id;
            const float gain = levels.master_mute || (is_phone ? levels.phone_mute : levels.desktop_mute) ? 0.0f :
                levels.master_gain * (is_phone ? levels.phone_gain : levels.desktop_gain);
            apply_levels(object, gain, is_phone);
            if (error.empty() && !object.volume_error.empty()) error = object.volume_error;
            else if (error.empty() && object.volume_pending) error = "Waiting for PipeWire to confirm software volume";
        }
    }
    if (graph_error[0]) error = graph_error;
    else if (error.empty() && enabled && !current.route_ready) error = "Waiting for direct stereo links to become ready";
    copy_text(current.last_error, sizeof(current.last_error), error.c_str());
}

template <typename Operation>
int boundary(char *error, size_t error_size, Operation operation) noexcept {
    try {
        operation();
        copy_text(error, error_size, "");
        return 0;
    } catch (const std::exception &exception) { copy_text(error, error_size, exception.what()); }
    catch (...) { copy_text(error, error_size, "Unexpected native audio engine error"); }
    return -1;
}

}

struct bab_engine {
    Engine value;
    explicit bab_engine(const bab_config &config) : value(config) {}
};

extern "C" bab_engine *bab_engine_create(const bab_config *config, char *error, size_t error_size) {
    bab_engine *result = nullptr;
    boundary(error, error_size, [&] {
        if (!config) throw std::runtime_error("Missing engine config");
        auto engine = std::make_unique<bab_engine>(*config);
        engine->value.init();
        result = engine.release();
    });
    return result;
}

extern "C" int bab_engine_set_levels(bab_engine *engine, const bab_levels *levels, char *error, size_t error_size) {
    return boundary(error, error_size, [&] {
        if (!engine || !levels) throw std::runtime_error("Missing engine or levels");
        for (const auto gain : {levels->phone_gain, levels->desktop_gain, levels->master_gain})
            if (!std::isfinite(gain) || gain < 0.0f || gain > 1.0f) throw std::runtime_error("Gain must be a finite value from 0.0 to 1.0");
        auto &value = engine->value;
        LoopLock lock(value.loop);
        value.flush();
        const auto previous = value.levels;
        value.levels = *levels;
        for (auto &entry : value.objects) {
            auto &object = *entry.second;
            if (object.external_volume || (object.volume_pending && !object.volume_error.empty()))
                value.restore_levels(object);
            object.volume_error.clear();
        }
        value.graph_error[0] = '\0';
        value.reconcile();
        value.flush();
        value.reconcile();
        for (const auto id : value.level_targets) {
            const auto &object = *value.objects.at(id);
            if (!object.volume_error.empty()) throw std::runtime_error(object.volume_error);
            if (object.volume_pending) throw std::runtime_error("PipeWire has not confirmed the requested software volume");
        }
        if (value.graph_error[0]) throw std::runtime_error(value.graph_error);
        if (value.enabled) {
            const bool phone_changed = previous.phone_gain != levels->phone_gain || previous.phone_mute != levels->phone_mute;
            const bool desktop_changed = previous.desktop_gain != levels->desktop_gain || previous.desktop_mute != levels->desktop_mute;
            const bool master_changed = previous.master_gain != levels->master_gain || previous.master_mute != levels->master_mute;
            const bool phone_target = value.level_targets.count(value.phone_id) != 0;
            const bool desktop_target = std::any_of(value.level_targets.begin(), value.level_targets.end(),
                [&value](uint32_t id) { return id != value.phone_id; });
            if (phone_changed && !phone_target)
                throw std::runtime_error("No controlled iPhone stream is available; requested levels are pending until its direct route is ready");
            if (desktop_changed && !desktop_target)
                throw std::runtime_error("No controllable desktop playback stream targets the selected headphones; requested levels are pending until one is available");
            if (master_changed && value.level_targets.empty())
                throw std::runtime_error("No controlled playback streams are available; requested master levels are pending until playback is available");
        }
    });
}

extern "C" int bab_engine_set_enabled(bab_engine *engine, uint8_t enabled, char *error, size_t error_size) {
    return boundary(error, error_size, [&] {
        if (!engine) throw std::runtime_error("Missing engine");
        LoopLock lock(engine->value.loop);
        engine->value.flush();
        engine->value.enabled = enabled != 0;
        engine->value.reconcile();
        engine->value.flush();
        engine->value.reconcile();
    });
}

extern "C" int bab_engine_tick(bab_engine *engine, char *error, size_t error_size) {
    return boundary(error, error_size, [&] {
        if (!engine) throw std::runtime_error("Missing engine");
        LoopLock lock(engine->value.loop);
        engine->value.reconcile();
    });
}

extern "C" void bab_engine_status(const bab_engine *engine, bab_status *status) {
    if (!status) return;
    *status = {};
    try {
        if (!engine) return;
        LoopLock lock(engine->value.loop);
        *status = engine->value.current;
        if (!engine->value.connected || engine->value.fatal) {
            status->pipewire_connected = false;
            status->phone_ready = false;
            status->headphones_ready = false;
            status->route_ready = false;
            status->phone_policy_ready = false;
            copy_text(status->last_error, sizeof(status->last_error), engine->value.graph_error);
        }
    } catch (...) { copy_text(status->last_error, sizeof(status->last_error), "Cannot read native audio status"); }
}

extern "C" void bab_engine_destroy(bab_engine *engine) {
    try { delete engine; } catch (...) {}
}
