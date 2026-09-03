#include "bluetooth_audio_bridge.h"

#include <pipewire/pipewire.h>
#include <pipewire/filter.h>
#include <spa/param/param.h>
#include <spa/pod/builder.h>
#include <spa/pod/parser.h>

#include <algorithm>
#include <array>
#include <atomic>
#include <cerrno>
#include <cctype>
#include <cmath>
#include <cstring>
#include <limits>
#include <map>
#include <memory>
#include <mutex>
#include <set>
#include <stdexcept>
#include <string>
#include <utility>

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

bool a2dp(const std::string &profile) {
    return profile.compare(0, 5, "a2dp-") == 0;
}

struct Engine;

enum class Kind { Node, Port, Device, Link };

struct Profile {
    int index = -1;
    std::string name;
    uint32_t available = SPA_PARAM_AVAILABILITY_unknown;
};

struct Object {
    Engine *engine;
    uint32_t id;
    Kind kind;
    Properties props;
    pw_proxy *proxy = nullptr;
    spa_hook listener{};
    bool listening = false;
    pw_node_state node_state = PW_NODE_STATE_CREATING;
    pw_link_state link_state = PW_LINK_STATE_INIT;
    std::map<int, Profile> profiles;
    std::string active_profile;
    int requested_aac = -1;
    int profile_sequence = 0;
    bool subscribed = false;

    Object(Engine *owner, uint32_t global_id, Kind type) : engine(owner), id(global_id), kind(type) {}
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
    std::string sink_name;
    std::string phone_address;
    std::string headphones_address;
    bool fallback;
    pw_thread_loop *loop = nullptr;
    pw_context *context = nullptr;
    pw_core *core = nullptr;
    pw_registry *registry = nullptr;
    pw_filter *filter = nullptr;
    spa_hook core_listener{};
    spa_hook registry_listener{};
    spa_hook filter_listener{};
    bool started = false;
    bool connected = false;
    bool fatal = false;
    bool enabled = false;
    uint32_t phone_id = invalid_id;
    uint32_t headphones_id = invalid_id;
    uint32_t filter_id = invalid_id;
    std::array<std::array<void *, 2>, 3> ports{};
    std::unique_ptr<Owned> sink;
    std::map<uint32_t, std::unique_ptr<Object>> objects;
    std::map<Route, std::unique_ptr<Owned>> links;
    std::atomic<float> phone_gain{0.5f};
    std::atomic<float> desktop_gain{0.5f};
    std::atomic<float> master_gain{0.8f};
    std::atomic<bool> output_gate{false};
    std::atomic<bool> phone_gate{false};
    std::atomic<uint32_t> sample_rate{0};
    bab_status current{};
    char graph_error[512]{};

    explicit Engine(const bab_config &config)
        : sink_name(config.virtual_sink_name ? config.virtual_sink_name : ""),
          phone_address(address(config.iphone_address ? config.iphone_address : "")),
          headphones_address(address(config.headphones_address ? config.headphones_address : "")),
          fallback(config.allow_codec_fallback != 0) {
        if (sink_name.empty() || phone_address.empty() || headphones_address.empty())
            throw std::runtime_error("A virtual sink name and two valid Bluetooth addresses are required");
        if (phone_address == headphones_address)
            throw std::runtime_error("The iPhone and headphones must be different devices");
    }

    ~Engine() {
        output_gate.store(false);
        phone_gate.store(false);
        if (started) pw_thread_loop_stop(loop);
        links.clear();
        if (filter) {
            spa_hook_remove(&filter_listener);
            pw_filter_destroy(filter);
        }
        sink.reset();
        objects.clear();
        if (registry) {
            spa_hook_remove(&registry_listener);
            pw_proxy_destroy(reinterpret_cast<pw_proxy *>(registry));
        }
        if (core) {
            spa_hook_remove(&core_listener);
            pw_core_disconnect(core);
        }
        if (context) pw_context_destroy(context);
        if (loop) pw_thread_loop_destroy(loop);
    }

    static void registry_global(void *, uint32_t, uint32_t, const char *, uint32_t, const spa_dict *) noexcept;
    static void registry_remove(void *, uint32_t) noexcept;
    static void node_info(void *, const pw_node_info *) noexcept;
    static void port_info(void *, const pw_port_info *) noexcept;
    static void device_info(void *, const pw_device_info *) noexcept;
    static void device_param(void *, int, uint32_t, uint32_t, uint32_t, const spa_pod *) noexcept;
    static void link_info(void *, const pw_link_info *) noexcept;
    static void process(void *, spa_io_position *) noexcept;
    void init();
    void reconcile();
    uint32_t find_port(uint32_t node, const char *direction, const char *channel, const char *name = nullptr) const;
    Object *device_for(const Object &node) const;
    bool matches(const Object &node, const std::string &expected) const;
    std::string profile_for(const Object &node) const;
    std::string codec_for(const Object &node) const;
    bool request_aac(Object &headphones);
    void add_link(const Route &route);
    bool link_ready(const Route &route) const;
    bool foreign_link(uint32_t output_node, uint32_t allowed_input) const;
    void callback_failure() noexcept {
        fatal = true;
        output_gate.store(false);
        phone_gate.store(false);
        copy_text(graph_error, sizeof(graph_error), "PipeWire event processing failed");
    }
};

static_assert(std::atomic<float>::is_always_lock_free, "RT gains require lock-free atomics");
static_assert(std::atomic<bool>::is_always_lock_free, "RT gates require lock-free atomics");
static_assert(std::atomic<uint32_t>::is_always_lock_free, "RT rate requires lock-free atomics");

const pw_proxy_events owned_events = [] {
    pw_proxy_events events{};
    events.version = PW_VERSION_PROXY_EVENTS;
    events.bound = [](void *data, uint32_t id) noexcept { static_cast<Owned *>(data)->id = id; };
    events.removed = [](void *data) noexcept {
        auto &owned = *static_cast<Owned *>(data);
        owned.failed = true;
        owned.engine->output_gate.store(false);
    };
    events.error = [](void *data, int, int, const char *message) noexcept {
        auto &owned = *static_cast<Owned *>(data);
        owned.failed = true;
        owned.engine->output_gate.store(false);
        copy_text(owned.engine->graph_error, sizeof(owned.engine->graph_error), message);
    };
    return events;
}();

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
        events.error = [](void *data, uint32_t id, int, int result, const char *message) noexcept {
            auto &engine = *static_cast<Engine *>(data);
            if (id == PW_ID_CORE || result == -EPIPE || result == -ECONNRESET) {
                engine.connected = false;
                engine.fatal = true;
            }
            engine.output_gate.store(false);
            copy_text(engine.graph_error, sizeof(engine.graph_error), message);
        };
        return events;
    }();
    pw_core_add_listener(core, &core_listener, &core_events, this);
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

    sink = std::make_unique<Owned>(this);
    auto *properties = pw_properties_new(
        "factory.name", "support.null-audio-sink",
        PW_KEY_NODE_NAME, sink_name.c_str(),
        PW_KEY_NODE_DESCRIPTION, "Bluetooth Audio Bridge",
        PW_KEY_MEDIA_CLASS, "Audio/Sink",
        PW_KEY_NODE_VIRTUAL, "true",
        "node.autoconnect", "false", "node.dont-fallback", "true",
        "priority.session", "0", "priority.driver", "0",
        "audio.channels", "2", "audio.position", "[ FL FR ]",
        "monitor.channel-volumes", "false",
        "adapter.auto-port-config", "{ mode = dsp monitor = true position = preserve }",
        "object.linger", "false", nullptr);
    if (!properties) throw std::bad_alloc();
    sink->proxy = reinterpret_cast<pw_proxy *>(pw_core_create_object(core, "adapter", PW_TYPE_INTERFACE_Node,
        PW_VERSION_NODE, &properties->dict, 0));
    pw_properties_free(properties);
    if (!sink->proxy) throw std::runtime_error("Cannot create the Bluetooth Audio Bridge virtual sink");
    pw_proxy_add_listener(sink->proxy, &sink->listener, &owned_events, sink.get());
    sink->listening = true;

    const auto filter_name = sink_name + ".mixer";
    filter = pw_filter_new(core, "Bluetooth Audio Bridge mixer", pw_properties_new(
        PW_KEY_NODE_NAME, filter_name.c_str(), PW_KEY_MEDIA_CLASS, "Audio/Filter",
        PW_KEY_NODE_VIRTUAL, "true", "node.autoconnect", "false", "node.dont-fallback", "true",
        "node.dont-reconnect", "true", "node.passive", "true", nullptr));
    if (!filter) throw std::runtime_error("Cannot create the PipeWire mixer");
    static const pw_filter_events filter_events = [] {
        pw_filter_events events{};
        events.version = PW_VERSION_FILTER_EVENTS;
        events.process = process;
        events.state_changed = [](void *data, pw_filter_state, pw_filter_state state, const char *error) noexcept {
            if (state != PW_FILTER_STATE_ERROR) return;
            auto &engine = *static_cast<Engine *>(data);
            engine.fatal = true;
            engine.output_gate.store(false);
            copy_text(engine.graph_error, sizeof(engine.graph_error), error);
        };
        return events;
    }();
    pw_filter_add_listener(filter, &filter_listener, &filter_events, this);
    const char *names[3][2] = {{"desktop_FL", "desktop_FR"}, {"phone_FL", "phone_FR"}, {"output_FL", "output_FR"}};
    for (size_t group = 0; group < ports.size(); ++group) {
        for (size_t channel = 0; channel < 2; ++channel) {
            ports[group][channel] = pw_filter_add_port(filter, group == 2 ? PW_DIRECTION_OUTPUT : PW_DIRECTION_INPUT,
                PW_FILTER_PORT_FLAG_MAP_BUFFERS, sizeof(uint32_t), pw_properties_new(
                    PW_KEY_FORMAT_DSP, "32 bit float mono audio", PW_KEY_PORT_NAME, names[group][channel],
                    "audio.channel", channel == 0 ? "FL" : "FR", nullptr), nullptr, 0);
            if (!ports[group][channel]) throw std::runtime_error("Cannot create stereo PipeWire mixer ports");
        }
    }
    if (pw_filter_connect(filter, PW_FILTER_FLAG_RT_PROCESS, nullptr, 0) < 0)
        throw std::runtime_error("Cannot connect the PipeWire mixer");
    if (pw_thread_loop_start(loop) < 0) throw std::runtime_error("Cannot start the PipeWire client loop");
    started = true;
}

void Engine::registry_global(void *data, uint32_t id, uint32_t, const char *type, uint32_t version, const spa_dict *props) noexcept {
    auto &engine = *static_cast<Engine *>(data);
    try {
        Kind kind;
        uint32_t supported;
        if (std::strcmp(type, PW_TYPE_INTERFACE_Node) == 0) { kind = Kind::Node; supported = PW_VERSION_NODE; }
        else if (std::strcmp(type, PW_TYPE_INTERFACE_Port) == 0) { kind = Kind::Port; supported = PW_VERSION_PORT; }
        else if (std::strcmp(type, PW_TYPE_INTERFACE_Device) == 0) { kind = Kind::Device; supported = PW_VERSION_DEVICE; }
        else if (std::strcmp(type, PW_TYPE_INTERFACE_Link) == 0) { kind = Kind::Link; supported = PW_VERSION_LINK; }
        else return;
        auto object = std::make_unique<Object>(&engine, id, kind);
        merge(object->props, props);
        object->proxy = reinterpret_cast<pw_proxy *>(pw_registry_bind(engine.registry, id, type, std::min(version, supported), 0));
        if (!object->proxy) return;
        auto *pointer = object.get();
        engine.objects[id] = std::move(object);
        if (kind == Kind::Node) {
            static const pw_node_events events = [] { pw_node_events e{}; e.version = PW_VERSION_NODE_EVENTS; e.info = node_info; return e; }();
            pw_node_add_listener(reinterpret_cast<pw_node *>(pointer->proxy), &pointer->listener, &events, pointer);
        } else if (kind == Kind::Port) {
            static const pw_port_events events = [] { pw_port_events e{}; e.version = PW_VERSION_PORT_EVENTS; e.info = port_info; return e; }();
            pw_port_add_listener(reinterpret_cast<pw_port *>(pointer->proxy), &pointer->listener, &events, pointer);
        } else if (kind == Kind::Device) {
            static const pw_device_events events = [] { pw_device_events e{}; e.version = PW_VERSION_DEVICE_EVENTS; e.info = device_info; e.param = device_param; return e; }();
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
    if (id == engine.headphones_id || id == engine.filter_id || (engine.sink && id == engine.sink->id))
        engine.output_gate.store(false);
    if (id == engine.phone_id) engine.phone_gate.store(false);
    for (auto &entry : engine.links) {
        if (entry.second->id == id) {
            entry.second->failed = true;
            engine.output_gate.store(false);
        }
    }
    engine.objects.erase(id);
}

void Engine::node_info(void *data, const pw_node_info *info) noexcept {
    auto &object = *static_cast<Object *>(data);
    try {
        if (info->change_mask & PW_NODE_CHANGE_MASK_PROPS) {
            merge(object.props, info->props);
            if (object.id == object.engine->headphones_id) object.engine->output_gate.store(false);
            if (object.id == object.engine->phone_id) object.engine->phone_gate.store(false);
        }
        if (info->change_mask & PW_NODE_CHANGE_MASK_STATE) object.node_state = info->state;
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
    try {
        const auto connection = property(object.props, "api.bluez5.connection");
        merge(object.props, info->props);
        if (!matches_address(object.props, object.engine->headphones_address)) return;
        if (connection == "disconnected" && property(object.props, "api.bluez5.connection") == "connected") object.requested_aac = -1;
        if (!object.subscribed) {
            uint32_t params[] = {SPA_PARAM_Profile};
            pw_device_subscribe_params(reinterpret_cast<pw_device *>(object.proxy), params, 1);
            object.subscribed = true;
        }
        if (info->change_mask & PW_DEVICE_CHANGE_MASK_PARAMS) {
            for (uint32_t index = 0; index < info->n_params; ++index) {
                if (info->params[index].id != SPA_PARAM_EnumProfile) continue;
                object.profiles.clear();
                object.profile_sequence++;
                pw_device_enum_params(reinterpret_cast<pw_device *>(object.proxy), object.profile_sequence,
                    SPA_PARAM_EnumProfile, 0, std::numeric_limits<uint32_t>::max(), nullptr);
            }
        }
    } catch (...) { object.engine->callback_failure(); }
}

void Engine::device_param(void *data, int sequence, uint32_t id, uint32_t, uint32_t, const spa_pod *param) noexcept {
    auto &object = *static_cast<Object *>(data);
    try {
        if (!param || (id != SPA_PARAM_Profile && id != SPA_PARAM_EnumProfile)) return;
        int index = -1;
        const char *name = nullptr;
        uint32_t available = SPA_PARAM_AVAILABILITY_unknown;
        if (spa_pod_parse_object(param, SPA_TYPE_OBJECT_ParamProfile, nullptr,
            SPA_PARAM_PROFILE_index, SPA_POD_Int(&index), SPA_PARAM_PROFILE_name, SPA_POD_OPT_String(&name),
            SPA_PARAM_PROFILE_available, SPA_POD_OPT_Id(&available)) < 0) return;
        if (id == SPA_PARAM_Profile) {
            object.active_profile = name ? name : "";
            object.engine->output_gate.store(false);
        } else if (sequence == object.profile_sequence && name) {
            object.profiles[index] = Profile{index, name, available};
        }
    } catch (...) { object.engine->callback_failure(); }
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
                    object.engine->output_gate.store(false);
                    copy_text(object.engine->graph_error, sizeof(object.engine->graph_error), info->error);
                }
            }
        }
        if ((info->output_node_id == object.engine->phone_id && info->input_node_id != object.engine->filter_id) ||
            (info->output_node_id == object.engine->filter_id && info->input_node_id != object.engine->headphones_id)) {
            object.engine->phone_gate.store(false);
            object.engine->output_gate.store(false);
        }
    } catch (...) { object.engine->callback_failure(); }
}

void Engine::process(void *data, spa_io_position *position) noexcept {
    auto &engine = *static_cast<Engine *>(data);
    if (!position || position->clock.duration > std::numeric_limits<uint32_t>::max()) return;
    const auto frames = static_cast<uint32_t>(position->clock.duration);
    if (position->clock.rate.num != 0)
        engine.sample_rate.store(position->clock.rate.denom / position->clock.rate.num, std::memory_order_relaxed);
    const bool enabled = engine.output_gate.load(std::memory_order_acquire);
    const float phone_gain = engine.phone_gate.load(std::memory_order_acquire) ? engine.phone_gain.load(std::memory_order_relaxed) : 0.0f;
    const float desktop_gain = engine.desktop_gain.load(std::memory_order_relaxed);
    const float master_gain = engine.master_gain.load(std::memory_order_relaxed);
    // PipeWire จัดการ clock, resampling และ desktop mixing; callback นี้ควบคุม gain และ clipping เท่านั้น
    for (size_t channel = 0; channel < 2; ++channel) {
        auto *desktop = static_cast<const float *>(pw_filter_get_dsp_buffer(engine.ports[0][channel], frames));
        auto *phone = static_cast<const float *>(pw_filter_get_dsp_buffer(engine.ports[1][channel], frames));
        auto *output = static_cast<float *>(pw_filter_get_dsp_buffer(engine.ports[2][channel], frames));
        if (!output) continue;
        for (uint32_t index = 0; index < frames; ++index) {
            float sample = 0.0f;
            if (enabled) sample = ((desktop ? desktop[index] : 0.0f) * desktop_gain + (phone ? phone[index] : 0.0f) * phone_gain) * master_gain;
            output[index] = std::isfinite(sample) ? std::clamp(sample, -1.0f, 1.0f) : 0.0f;
        }
    }
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

std::string Engine::profile_for(const Object &node) const {
    auto profile = property(node.props, "api.bluez5.profile");
    if (profile.empty()) {
        const auto *device = device_for(node);
        if (device) profile = device->active_profile;
    }
    return profile;
}

std::string Engine::codec_for(const Object &node) const {
    // อ่านเฉพาะ codec ที่ transport รายงาน ไม่ใช้ bluez5.codecs หรือชื่อ profile แทนผล negotiation
    auto codec = property(node.props, "api.bluez5.codec");
    if (codec.empty()) {
        const auto *device = device_for(node);
        if (device) codec = property(device->props, "api.bluez5.codec");
    }
    std::transform(codec.begin(), codec.end(), codec.begin(), [](unsigned char c) { return static_cast<char>(std::tolower(c)); });
    return codec;
}

uint32_t Engine::find_port(uint32_t node, const char *direction, const char *channel, const char *name) const {
    if (node == invalid_id) return invalid_id;
    for (const auto &entry : objects) {
        const auto &object = *entry.second;
        if (object.kind == Kind::Port && number(property(object.props, "node.id")) == node &&
            property(object.props, "port.direction") == direction && property(object.props, "audio.channel") == channel &&
            (!name || property(object.props, "port.name") == name)) return object.id;
    }
    return invalid_id;
}

bool Engine::request_aac(Object &headphones) {
    auto *device = device_for(headphones);
    if (!device || !a2dp(profile_for(headphones)) || codec_for(headphones) == "aac") return false;
    for (const auto &entry : device->profiles) {
        const auto &profile = entry.second;
        if (profile.name != "a2dp-sink-aac" || profile.available == SPA_PARAM_AVAILABILITY_no || device->requested_aac == profile.index) continue;
        uint8_t buffer[256];
        spa_pod_builder builder = SPA_POD_BUILDER_INIT(buffer, sizeof(buffer));
        const auto *param = static_cast<const spa_pod *>(spa_pod_builder_add_object(&builder, SPA_TYPE_OBJECT_ParamProfile, SPA_PARAM_Profile,
            SPA_PARAM_PROFILE_index, SPA_POD_Int(profile.index), SPA_PARAM_PROFILE_save, SPA_POD_Bool(false)));
        device->requested_aac = profile.index;
        output_gate.store(false);
        if (pw_device_set_param(reinterpret_cast<pw_device *>(device->proxy), SPA_PARAM_Profile, 0, param) < 0) {
            copy_text(graph_error, sizeof(graph_error), "The selected headphones rejected the advertised A2DP AAC profile");
            return false;
        }
        return true;
    }
    return false;
}

bool Engine::foreign_link(uint32_t output_node, uint32_t allowed_input) const {
    if (output_node == invalid_id) return false;
    for (const auto &entry : objects) {
        const auto &object = *entry.second;
        if (object.kind != Kind::Link || object.link_state < PW_LINK_STATE_INIT ||
            number(property(object.props, "link.output.node")) != output_node) continue;
        if (number(property(object.props, "link.input.node")) != allowed_input) return true;
        const auto owned = links.find({number(property(object.props, "link.output.port")), number(property(object.props, "link.input.port"))});
        if (owned == links.end() || (owned->second->id != invalid_id && owned->second->id != object.id)) return true;
    }
    return false;
}

void Engine::add_link(const Route &route) {
    auto output = objects.find(route.first);
    auto input = objects.find(route.second);
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
    if (!link->proxy) throw std::runtime_error("Cannot create an explicit bridge audio link");
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

void Engine::reconcile() {
    if (fatal || (sink && sink->failed)) {
        output_gate.store(false);
        phone_gate.store(false);
        throw std::runtime_error(graph_error[0] ? graph_error : "The PipeWire bridge graph is unavailable");
    }
    current = {};
    current.pipewire_connected = connected;
    current.routing_enabled = enabled;
    current.sample_rate = sample_rate.load(std::memory_order_relaxed);
    current.channels = 2;
    copy_text(current.phone_stream_state, sizeof(current.phone_stream_state), "disconnected");
    copy_text(current.output_stream_state, sizeof(current.output_stream_state), "disconnected");
    if (!connected) {
        copy_text(current.last_error, sizeof(current.last_error), "Waiting for the PipeWire connection");
        return;
    }
    filter_id = pw_filter_get_node_id(filter);
    Object *phone = nullptr;
    Object *headphones = nullptr;
    bool unsafe_phone = false;
    bool multiple_phones = false;
    bool multiple_headphones = false;
    for (auto &entry : objects) {
        auto &object = *entry.second;
        if (object.kind != Kind::Node) continue;
        const auto &media_class = property(object.props, "media.class");
        if (matches(object, phone_address)) {
            if (media_class == "Stream/Output/Audio" || property(object.props, "node.autoconnect") == "true") unsafe_phone = true;
            if (media_class == "Audio/Source" || media_class == "Audio/Source/Virtual") {
                if (phone) multiple_phones = true;
                else phone = &object;
            }
        }
        if (media_class == "Audio/Sink" && matches(object, headphones_address)) {
            if (headphones) multiple_headphones = true;
            else headphones = &object;
        }
    }
    phone_id = phone ? phone->id : invalid_id;
    headphones_id = headphones ? headphones->id : invalid_id;
    std::set<Route> desktop_routes;
    std::set<Route> phone_routes;
    std::set<Route> output_routes;
    const char *channels[2] = {"FL", "FR"};
    const char *desktop_names[2] = {"desktop_FL", "desktop_FR"};
    const char *phone_names[2] = {"phone_FL", "phone_FR"};
    const char *output_names[2] = {"output_FL", "output_FR"};
    for (size_t channel = 0; channel < 2; ++channel) {
        const auto desktop_output = find_port(sink->id, "out", channels[channel]);
        const auto desktop_input = find_port(filter_id, "in", channels[channel], desktop_names[channel]);
        const auto phone_output = find_port(phone_id, "out", channels[channel]);
        const auto phone_input = find_port(filter_id, "in", channels[channel], phone_names[channel]);
        const auto mixed_output = find_port(filter_id, "out", channels[channel], output_names[channel]);
        const auto headphones_input = find_port(headphones_id, "in", channels[channel]);
        if (desktop_output != invalid_id && desktop_input != invalid_id) desktop_routes.emplace(desktop_output, desktop_input);
        if (phone_output != invalid_id && phone_input != invalid_id) phone_routes.emplace(phone_output, phone_input);
        if (mixed_output != invalid_id && headphones_input != invalid_id) output_routes.emplace(mixed_output, headphones_input);
    }
    current.virtual_sink_ready = sink->id != invalid_id && objects.count(sink->id) && desktop_routes.size() == 2;
    std::string phone_error;
    const bool external_phone_route = foreign_link(phone_id, filter_id);
    if (unsafe_phone) phone_error = "Unsafe automatic iPhone playback detected. Apply the supplied WirePlumber input policy and reconnect the iPhone before enabling the bridge.";
    else if (multiple_phones) phone_error = "More than one iPhone source matches the configured address; refusing ambiguous routing";
    else if (phone) {
        copy_text(current.phone_stream_state, sizeof(current.phone_stream_state), pw_node_state_as_string(phone->node_state));
        if (property(phone->props, "bluetooth-audio-bridge.phone") != "true" || property(phone->props, "node.autoconnect") != "false")
            phone_error = "The selected iPhone source needs the supplied WirePlumber input policy (bluetooth-audio-bridge.phone=true and node.autoconnect=false)";
        else if (!a2dp(profile_for(*phone))) phone_error = "The selected iPhone source has no verified A2DP profile";
        else if (external_phone_route) phone_error = "The selected iPhone already has playback links outside the bridge; remove that automatic route to avoid duplicate or speaker playback";
        else if (phone_routes.size() != 2) phone_error = "Waiting for the selected iPhone stereo output ports";
        else if (phone->node_state == PW_NODE_STATE_ERROR) phone_error = "The selected iPhone audio node is in an error state";
        else current.phone_ready = true;
    }
    bool selecting_aac = false;
    std::string output_error;
    if (!headphones) output_error = "Waiting for the configured Bluetooth headphones";
    else {
        const auto codec = codec_for(*headphones);
        copy_text(current.codec, sizeof(current.codec), codec.empty() ? "unknown" : codec.c_str());
        copy_text(current.output_stream_state, sizeof(current.output_stream_state), pw_node_state_as_string(headphones->node_state));
        if (multiple_headphones) output_error = "More than one headphone sink matches the configured address; refusing ambiguous routing";
        else if (!a2dp(profile_for(*headphones))) output_error = "The selected headphones are not using a verified A2DP profile; choose A2DP without changing the existing microphone service";
        else if (enabled && (selecting_aac = request_aac(*headphones))) output_error = "Selecting the advertised A2DP AAC profile; waiting for negotiated codec information";
        else if (codec != "aac" && !fallback) output_error = codec.empty() ? "AAC is required, but the selected headphones do not report a negotiated codec" : "AAC is required; the selected headphones negotiated " + codec + ". Enable codec fallback explicitly to allow another A2DP codec";
        else if (output_routes.size() != 2) output_error = "Waiting for the selected headphone stereo input ports";
        else if (headphones->node_state == PW_NODE_STATE_ERROR) output_error = "The selected headphone node is in an error state";
        else current.headphones_ready = true;
    }
    if (foreign_link(filter_id, headphones_id)) {
        current.headphones_ready = false;
        output_error = "The bridge mixer has an unexpected external output link; remove that link before routing private audio";
    }
    if (unsafe_phone || multiple_phones || external_phone_route) current.headphones_ready = false;
    std::set<Route> desired = desktop_routes;
    if (enabled && current.phone_ready) desired.insert(phone_routes.begin(), phone_routes.end());
    if (enabled && current.headphones_ready && !selecting_aac) desired.insert(output_routes.begin(), output_routes.end());
    for (auto it = links.begin(); it != links.end();) {
        if (!desired.count(it->first) || it->second->failed) {
            output_gate.store(false);
            it = links.erase(it);
        } else ++it;
    }
    for (const auto &route : desired) if (!links.count(route)) add_link(route);
    const auto ready = [this](const std::set<Route> &routes) {
        return routes.size() == 2 && std::all_of(routes.begin(), routes.end(), [this](const Route &route) { return link_ready(route); });
    };
    phone_gate.store(enabled && current.phone_ready && ready(phone_routes), std::memory_order_release);
    output_gate.store(enabled && current.headphones_ready && ready(output_routes) && ready(desktop_routes), std::memory_order_release);
    if (!enabled) copy_text(current.output_stream_state, sizeof(current.output_stream_state), "disabled");
    else if (current.headphones_ready && !output_gate.load()) copy_text(current.output_stream_state, sizeof(current.output_stream_state), "connecting");
    if (graph_error[0]) {
        copy_text(current.last_error, sizeof(current.last_error), graph_error);
        graph_error[0] = '\0';
    } else if (!phone_error.empty()) copy_text(current.last_error, sizeof(current.last_error), phone_error.c_str());
    else if (!output_error.empty()) copy_text(current.last_error, sizeof(current.last_error), output_error.c_str());
    else if (!current.virtual_sink_ready) copy_text(current.last_error, sizeof(current.last_error), "Waiting for the virtual desktop sink and mixer ports");
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

} // namespace

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
        engine->value.phone_gain.store(levels->phone_mute ? 0.0f : levels->phone_gain);
        engine->value.desktop_gain.store(levels->desktop_mute ? 0.0f : levels->desktop_gain);
        engine->value.master_gain.store(levels->master_mute ? 0.0f : levels->master_gain);
    });
}

extern "C" int bab_engine_set_enabled(bab_engine *engine, uint8_t enabled, char *error, size_t error_size) {
    return boundary(error, error_size, [&] {
        if (!engine) throw std::runtime_error("Missing engine");
        LoopLock lock(engine->value.loop);
        engine->value.enabled = enabled != 0;
        if (!enabled) {
            engine->value.output_gate.store(false);
            engine->value.phone_gate.store(false);
        }
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
            status->virtual_sink_ready = false;
            copy_text(status->last_error, sizeof(status->last_error), engine->value.graph_error);
        }
    } catch (...) { copy_text(status->last_error, sizeof(status->last_error), "Cannot read native audio status"); }
}

extern "C" void bab_engine_destroy(bab_engine *engine) {
    try { delete engine; } catch (...) {}
}
