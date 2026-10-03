#include "nimble_shim.h"

#include <assert.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>

struct ble_hs_cfg ble_hs_cfg;

static unsigned len_calls;
static unsigned copy_calls;
static unsigned append_calls;
static unsigned free_calls;
static const struct os_mbuf *seen_const_mbuf;
static struct os_mbuf *seen_mbuf;
static const void *seen_source;
static void *seen_destination;
static int seen_offset;
static int seen_length;
static uint16_t seen_append_length;
static int copy_result;
static int append_result;
static int free_result;
static unsigned sync_first_calls;
static unsigned sync_second_calls;
static int reset_first_reason;
static int reset_second_reason;
static unsigned register_first_calls;
static unsigned register_second_calls;
static struct ble_gatt_register_ctxt *seen_register_context;
static void *seen_register_arg;

static void
sync_first(void)
{
    ++sync_first_calls;
}

static void
sync_second(void)
{
    ++sync_second_calls;
}

static void
reset_first(int reason)
{
    reset_first_reason = reason;
}

static void
reset_second(int reason)
{
    reset_second_reason = reason;
}

static void
register_first(struct ble_gatt_register_ctxt *context, void *callback_arg)
{
    ++register_first_calls;
    seen_register_context = context;
    seen_register_arg = callback_arg;
}

static void
register_second(struct ble_gatt_register_ctxt *context, void *callback_arg)
{
    ++register_second_calls;
    seen_register_context = context;
    seen_register_arg = callback_arg;
}

uint16_t
os_mbuf_len(const struct os_mbuf *mbuf)
{
    uint32_t total = 0;
    ++len_calls;
    for (const struct os_mbuf *part = mbuf; part != NULL; part = part->next) {
        total += part->segment_len;
    }
    assert(total <= UINT16_MAX);
    return (uint16_t)total;
}

int
os_mbuf_copydata(const struct os_mbuf *mbuf, int offset, int length,
                 void *destination)
{
    ++copy_calls;
    seen_const_mbuf = mbuf;
    seen_offset = offset;
    seen_length = length;
    seen_destination = destination;
    return copy_result;
}

int
os_mbuf_append(struct os_mbuf *mbuf, const void *source, uint16_t length)
{
    ++append_calls;
    seen_mbuf = mbuf;
    seen_source = source;
    seen_append_length = length;
    return append_result;
}

int
os_mbuf_free_chain(struct os_mbuf *mbuf)
{
    ++free_calls;
    seen_mbuf = mbuf;
    for (struct os_mbuf *part = mbuf; part != NULL; part = part->next) {
        assert(part->consumed == 0);
        part->consumed = 1;
    }
    return free_result;
}

static void
assert_view(const struct argyle_nimble_gap_event_view *view, uint8_t type,
            uint8_t subscribe_reason, uint8_t notify_enabled,
            uint8_t indicate_enabled, uint16_t conn_handle,
            uint16_t attr_handle, uint16_t mtu, uint16_t channel_id,
            int status, int reason, uint8_t indication)
{
    assert(view->type == type);
    assert(view->subscribe_reason == subscribe_reason);
    assert(view->notify_enabled == notify_enabled);
    assert(view->indicate_enabled == indicate_enabled);
    assert(view->conn_handle == conn_handle);
    assert(view->attr_handle == attr_handle);
    assert(view->mtu == mtu);
    assert(view->channel_id == channel_id);
    assert(view->status == status);
    assert(view->reason == reason);
    assert(view->indication == indication);
}

static void
test_callback_setters(void)
{
    ble_hs_cfg = (struct ble_hs_cfg){0};

    argyle_nimble_set_sync_callback(&sync_first);
    assert(ble_hs_cfg.sync_cb == &sync_first);
    ble_hs_cfg.sync_cb();
    assert(sync_first_calls == 1);
    argyle_nimble_set_sync_callback(&sync_second);
    assert(ble_hs_cfg.sync_cb == &sync_second);
    ble_hs_cfg.sync_cb();
    assert(sync_second_calls == 1);
    argyle_nimble_set_sync_callback(NULL);
    assert(ble_hs_cfg.sync_cb == NULL);

    argyle_nimble_set_reset_callback(&reset_first);
    assert(ble_hs_cfg.reset_cb == &reset_first);
    ble_hs_cfg.reset_cb(23);
    assert(reset_first_reason == 23);
    argyle_nimble_set_reset_callback(&reset_second);
    assert(ble_hs_cfg.reset_cb == &reset_second);
    ble_hs_cfg.reset_cb(-29);
    assert(reset_second_reason == -29);
    argyle_nimble_set_reset_callback(NULL);
    assert(ble_hs_cfg.reset_cb == NULL);

    int first_argument = 31;
    int second_argument = 37;
    struct ble_gatt_register_ctxt context = {.marker = 43};
    argyle_nimble_set_gatts_register_callback(&register_first,
                                               &first_argument);
    assert(ble_hs_cfg.gatts_register_cb == &register_first);
    assert(ble_hs_cfg.gatts_register_arg == &first_argument);
    ble_hs_cfg.gatts_register_cb(&context, ble_hs_cfg.gatts_register_arg);
    assert(register_first_calls == 1);
    assert(seen_register_context == &context);
    assert(seen_register_arg == &first_argument);
    argyle_nimble_set_gatts_register_callback(&register_second,
                                               &second_argument);
    assert(ble_hs_cfg.gatts_register_cb == &register_second);
    assert(ble_hs_cfg.gatts_register_arg == &second_argument);
    ble_hs_cfg.gatts_register_cb(&context, ble_hs_cfg.gatts_register_arg);
    assert(register_second_calls == 1);
    assert(seen_register_context == &context);
    assert(seen_register_arg == &second_argument);
    argyle_nimble_set_gatts_register_callback(NULL, NULL);
    assert(ble_hs_cfg.gatts_register_cb == NULL);
    assert(ble_hs_cfg.gatts_register_arg == NULL);
}

static void
test_sdk_error_alias(void)
{
    assert(BLE_HS_FOREVER == INT32_MAX);
    assert(ARGYLE_NIMBLE_HS_FOREVER == BLE_HS_FOREVER);
    assert(BLE_ERR_REM_USER_CONN_TERM == 19);
    assert(ARGYLE_NIMBLE_ERR_REM_USER_CONN_TERM == 19);
    assert(ARGYLE_NIMBLE_ERR_REM_USER_CONN_TERM == BLE_ERR_REM_USER_CONN_TERM);
}

static void
test_uuid_constructors_and_uuid128_errors(void)
{
    ble_uuid16_t uuid16 = argyle_nimble_uuid16(UINT16_C(0xbeef));
    assert(uuid16.u.type == BLE_UUID_TYPE_16);
    assert(uuid16.value == UINT16_C(0xbeef));

    ble_uuid32_t uuid32 = argyle_nimble_uuid32(UINT32_C(0x89abcdef));
    assert(uuid32.u.type == BLE_UUID_TYPE_32);
    assert(uuid32.value == UINT32_C(0x89abcdef));

    const uint8_t bytes[16] = {
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
        0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
    };
    ble_uuid128_t uuid128;
    memset(&uuid128, 0xa5, sizeof(uuid128));
    assert(argyle_nimble_uuid128(bytes, &uuid128) == 0);
    assert(uuid128.u.type == BLE_UUID_TYPE_128);
    assert(memcmp(uuid128.value, bytes, sizeof(bytes)) == 0);

    ble_uuid128_t unchanged;
    ble_uuid128_t before;
    memset(&unchanged, 0x5c, sizeof(unchanged));
    before = unchanged;
    assert(argyle_nimble_uuid128(NULL, &unchanged) == -1);
    assert(memcmp(&unchanged, &before, sizeof(unchanged)) == 0);
    assert(argyle_nimble_uuid128(bytes, NULL) == -1);
}

static void
test_gap_event_views(void)
{
    struct ble_gap_event event = {0};
    struct argyle_nimble_gap_event_view view;

    event.type = BLE_GAP_EVENT_CONNECT;
    event.connect.status = -17;
    event.connect.conn_handle = UINT16_C(0x1234);
    assert(argyle_nimble_gap_event_extract(&event, &view) == 0);
    assert_view(&view, BLE_GAP_EVENT_CONNECT, 0, 0, 0, UINT16_C(0x1234),
                0, 0, 0, -17, 0, 0);

    event = (struct ble_gap_event){0};
    event.type = BLE_GAP_EVENT_DISCONNECT;
    event.disconnect.reason = -31;
    event.disconnect.conn.conn_handle = UINT16_C(0x2345);
    assert(argyle_nimble_gap_event_extract(&event, &view) == 0);
    assert_view(&view, BLE_GAP_EVENT_DISCONNECT, 0, 0, 0,
                UINT16_C(0x2345), 0, 0, 0, 0, -31, 0);

    event = (struct ble_gap_event){0};
    event.type = BLE_GAP_EVENT_CONN_UPDATE;
    event.conn_update.status = 23;
    event.conn_update.conn_handle = UINT16_C(0x3456);
    assert(argyle_nimble_gap_event_extract(&event, &view) == 0);
    assert_view(&view, BLE_GAP_EVENT_CONN_UPDATE, 0, 0, 0,
                UINT16_C(0x3456), 0, 0, 0, 23, 0, 0);

    event = (struct ble_gap_event){0};
    event.type = BLE_GAP_EVENT_ADV_COMPLETE;
    event.adv_complete.reason = 41;
    assert(argyle_nimble_gap_event_extract(&event, &view) == 0);
    assert_view(&view, BLE_GAP_EVENT_ADV_COMPLETE, 0, 0, 0, 0, 0, 0, 0,
                0, 41, 0);

    event = (struct ble_gap_event){0};
    event.type = BLE_GAP_EVENT_NOTIFY_TX;
    event.notify_tx.status = -53;
    event.notify_tx.conn_handle = UINT16_C(0x4567);
    event.notify_tx.attr_handle = UINT16_C(0x5678);
    event.notify_tx.indication = 1;
    assert(argyle_nimble_gap_event_extract(&event, &view) == 0);
    assert_view(&view, BLE_GAP_EVENT_NOTIFY_TX, 0, 0, 0,
                UINT16_C(0x4567), UINT16_C(0x5678), 0, 0, -53, 0, 1);

    event = (struct ble_gap_event){0};
    event.type = BLE_GAP_EVENT_SUBSCRIBE;
    event.subscribe.conn_handle = UINT16_C(0x6789);
    event.subscribe.attr_handle = UINT16_C(0x789a);
    event.subscribe.reason = 2;
    event.subscribe.cur_notify = 1;
    event.subscribe.cur_indicate = 1;
    assert(argyle_nimble_gap_event_extract(&event, &view) == 0);
    assert_view(&view, BLE_GAP_EVENT_SUBSCRIBE, 2, 1, 1,
                UINT16_C(0x6789), UINT16_C(0x789a), 0, 0, 0, 0, 0);

    event = (struct ble_gap_event){0};
    event.type = BLE_GAP_EVENT_MTU;
    event.mtu.conn_handle = UINT16_C(0x89ab);
    event.mtu.channel_id = UINT16_C(0xcdef);
    event.mtu.value = UINT16_C(0x0247);
    assert(argyle_nimble_gap_event_extract(&event, &view) == 0);
    assert_view(&view, BLE_GAP_EVENT_MTU, 0, 0, 0, UINT16_C(0x89ab), 0,
                UINT16_C(0x0247), UINT16_C(0xcdef), 0, 0, 0);

    assert(argyle_nimble_gap_event_extract(NULL, &view) == -1);
    assert(argyle_nimble_gap_event_extract(&event, NULL) == -1);

    event.type = UINT8_C(0xfe);
    assert(argyle_nimble_gap_event_extract(&event, &view) == -1);
}

static void
test_mbuf_forwarding_and_guards(void)
{
    struct os_mbuf tail = {.next = NULL, .segment_len = 13, .consumed = 0};
    struct os_mbuf head = {.next = &tail, .segment_len = 29, .consumed = 0};
    assert(argyle_nimble_mbuf_len(&head) == 42);
    assert(len_calls == 1);

    uint8_t output[8] = {0};
    copy_result = 0;
    assert(argyle_nimble_mbuf_copydata(&head, 4, 6, output) == 0);
    assert(copy_calls == 1);
    assert(seen_const_mbuf == &head);
    assert(seen_offset == 4);
    assert(seen_length == 6);
    assert(seen_destination == output);

    copy_result = -23;
    assert(argyle_nimble_mbuf_copydata(&head, 9, 2, output) == -23);
    assert(copy_calls == 2);
    assert(seen_const_mbuf == &head);
    assert(seen_offset == 9);
    assert(seen_length == 2);
    assert(seen_destination == output);

    assert(argyle_nimble_mbuf_copydata(NULL, 0, 1, output) == -1);
    assert(argyle_nimble_mbuf_copydata(&head, -1, 1, output) == -1);
    assert(argyle_nimble_mbuf_copydata(&head, 0, -1, output) == -1);
    assert(argyle_nimble_mbuf_copydata(&head, 0, 1, NULL) == -1);
    assert(argyle_nimble_mbuf_copydata(&head, 0, 0, NULL) == 0);
    assert(copy_calls == 2);

    const uint8_t append_source[] = {0x31, 0x42, 0x53, 0x64};
    append_result = 0;
    assert(argyle_nimble_mbuf_append(&head, append_source,
                                     (uint16_t)sizeof(append_source)) == 0);
    assert(append_calls == 1);
    assert(seen_mbuf == &head);
    assert(seen_source == append_source);
    assert(seen_append_length == sizeof(append_source));

    append_result = -28;
    assert(argyle_nimble_mbuf_append(&head, append_source, 3) == -28);
    assert(append_calls == 2);
    assert(seen_mbuf == &head);
    assert(seen_source == append_source);
    assert(seen_append_length == 3);

    free_result = 0;
    assert(argyle_nimble_mbuf_free_chain(&head) == 0);
    assert(free_calls == 1);
    assert(seen_mbuf == &head);
    assert(head.consumed == 1);
    assert(tail.consumed == 1);

    struct os_mbuf failed = {.next = NULL, .segment_len = 5, .consumed = 0};
    free_result = -37;
    assert(argyle_nimble_mbuf_free_chain(&failed) == -37);
    assert(free_calls == 2);
    assert(seen_mbuf == &failed);
    assert(failed.consumed == 1);
}

int
main(void)
{
    test_callback_setters();
    test_sdk_error_alias();
    test_uuid_constructors_and_uuid128_errors();
    test_gap_event_views();
    test_mbuf_forwarding_and_guards();
    return 0;
}
