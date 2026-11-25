#ifndef JIMU_H
#define JIMU_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct JimuSession JimuSession;
typedef struct JimuHttpStream JimuHttpStream;
typedef struct JimuToolCompletion JimuToolCompletion;

typedef enum JimuStatus {
    JIMU_OK = 0,
    JIMU_ERR_INVALID_ARGUMENT = 1,
    JIMU_ERR_BUSY = 2,
    JIMU_ERR_CLOSED = 3,
    JIMU_ERR_NOT_FOUND = 4,
    JIMU_ERR_IO = 5,
    JIMU_ERR_INTERNAL = 6,
    JIMU_ERR_CANCELLED = 7,
    JIMU_ERR_BACKPRESSURE = 8
} JimuStatus;

typedef enum JimuHttpErrorKind {
    JIMU_HTTP_OK = 0,
    JIMU_HTTP_ERR_ABORTED = 1,
    JIMU_HTTP_ERR_TIMEOUT = 2,
    JIMU_HTTP_ERR_NETWORK = 3
} JimuHttpErrorKind;

/* ---- Host Streaming HTTP Transport Callbacks ---- */

typedef void (*JimuHttpStartFn)(
    void *ctx,
    uint64_t request_id,
    const char *request_json,
    const uint8_t *body_ptr,
    size_t body_len,
    JimuHttpStream *stream
);

typedef void (*JimuHttpCancelFn)(
    void *ctx,
    uint64_t request_id
);

typedef void (*JimuVoidDoneFn)(
    void *ctx
);

typedef struct JimuHttpCallbacks {
    void *ctx;
    JimuHttpStartFn start;
    JimuHttpCancelFn cancel;
    JimuVoidDoneFn destroy;
} JimuHttpCallbacks;

/* ---- Host Native Async Tool Callbacks ---- */

typedef void (*JimuToolExecuteFn)(
    void *ctx,
    uint64_t call_handle_id,
    const char *tool_call_id,
    const char *tool_name,
    const char *args_json,
    JimuToolCompletion *completion
);

typedef void (*JimuToolCancelFn)(
    void *ctx,
    uint64_t call_handle_id
);

typedef struct JimuToolCallbacks {
    void *ctx;
    JimuToolExecuteFn execute;
    JimuToolCancelFn cancel;
} JimuToolCallbacks;

/* ---- Event & Async Completion Callbacks ---- */

typedef void (*JimuEventFn)(
    void *ctx,
    const char *event_json
);

typedef void (*JimuStreamDoneFn)(
    void *ctx,
    JimuStatus status,
    const char *error_message
);

typedef void (*JimuResultDoneFn)(
    void *ctx,
    JimuStatus status,
    const char *result_json,
    const char *error_message
);

/* ---- Memory Management ---- */

void jimu_string_free(char *ptr);

/* ---- HTTP Stream Sink Functions (called by host) ---- */

JimuStatus jimu_http_on_response(
    JimuHttpStream *stream,
    uint16_t status_code,
    const char *headers_json
);

JimuStatus jimu_http_push_chunk(
    JimuHttpStream *stream,
    const uint8_t *data,
    size_t len
);

void jimu_http_finish(
    JimuHttpStream *stream,
    int32_t error_kind,
    const char *error_message
);

/* ---- Tool Completion Functions (called by host) ---- */

JimuStatus jimu_tool_update(
    JimuToolCompletion *completion,
    const char *result_json
);

void jimu_tool_complete(
    JimuToolCompletion *completion,
    const char *result_json,
    const char *error_message
);

/* ---- Independent Workspace Tools ---- */

typedef struct JimuBuiltinEnv JimuBuiltinEnv;
typedef struct JimuBuiltinTool JimuBuiltinTool;
typedef struct JimuBuiltinRun JimuBuiltinRun;
typedef struct JimuBuiltinEnvCompletion JimuBuiltinEnvCompletion;

typedef void (*JimuBuiltinEnvProviderFn)(void *ctx, JimuBuiltinEnvCompletion *completion);

typedef struct JimuBuiltinCallbacks {
    void *ctx;
    JimuBuiltinEnvProviderFn environment;
    JimuEventFn background_event;
    JimuVoidDoneFn destroy;
} JimuBuiltinCallbacks;

/* Config JSON takes cwd, optional max_output_bytes and default_exec_timeout_ms.
 * Either callbacks table may be NULL. On success the environment owns the callback
 * contexts until their destroy functions run. On failure ownership stays with the caller.
 * Background callbacks run on worker threads and must enqueue and return promptly. */
JimuBuiltinEnv *jimu_builtin_env_create(
    const char *config_json,
    const JimuBuiltinCallbacks *callbacks,
    const JimuHttpCallbacks *http_callbacks,
    char **out_error
);

/* Complete exactly once per environment request. Pass a JSON object of strings or NULL. */
void jimu_builtin_env_complete(JimuBuiltinEnvCompletion *completion, const char *environment_json);

/* Shutdown cancels active calls, reaps background processes and then calls on_done.
 * Destroy starts the same shutdown and frees the handle without waiting. */
void jimu_builtin_env_shutdown(JimuBuiltinEnv *env, void *ctx, JimuVoidDoneFn on_done);
void jimu_builtin_env_destroy(JimuBuiltinEnv *env);

JimuBuiltinTool *jimu_builtin_tool_read(JimuBuiltinEnv *env, char **out_error);
JimuBuiltinTool *jimu_builtin_tool_write(JimuBuiltinEnv *env, char **out_error);
JimuBuiltinTool *jimu_builtin_tool_edit(JimuBuiltinEnv *env, char **out_error);
JimuBuiltinTool *jimu_builtin_tool_apply_patch(JimuBuiltinEnv *env, char **out_error);
JimuBuiltinTool *jimu_builtin_tool_find(JimuBuiltinEnv *env, char **out_error);
JimuBuiltinTool *jimu_builtin_tool_grep(JimuBuiltinEnv *env, char **out_error);
JimuBuiltinTool *jimu_builtin_tool_sleep(JimuBuiltinEnv *env, char **out_error);
JimuBuiltinTool *jimu_builtin_tool_web_fetch(JimuBuiltinEnv *env, char **out_error);

/* Subprocess constructors report an invalid argument on iOS. */
JimuBuiltinTool *jimu_builtin_tool_exec_command_oneshot(JimuBuiltinEnv *env, char **out_error);
JimuBuiltinTool *jimu_builtin_tool_exec_command(JimuBuiltinEnv *env, char **out_error);
JimuBuiltinTool *jimu_builtin_tool_write_stdin(JimuBuiltinEnv *env, char **out_error);
JimuBuiltinTool *jimu_builtin_tool_monitor(JimuBuiltinEnv *env, char **out_error);

JimuBuiltinTool *jimu_builtin_tool_override(
    const JimuBuiltinTool *tool, const char *override_json, char **out_error
);
char *jimu_builtin_tool_definition_json(const JimuBuiltinTool *tool);
bool jimu_builtin_tool_is_sequential(const JimuBuiltinTool *tool);

/* A successful start calls done exactly once. The caller owns the run handle and
 * must destroy it, independently of tool lifetime. Progress and result JSON are
 * borrowed for the duration of each callback. */
JimuBuiltinRun *jimu_builtin_tool_execute(
    const JimuBuiltinTool *tool,
    const char *call_id,
    const char *arguments_json,
    void *ctx,
    JimuEventFn progress,
    JimuResultDoneFn done,
    char **out_error
);
void jimu_builtin_tool_cancel(const JimuBuiltinRun *run);
void jimu_builtin_run_destroy(JimuBuiltinRun *run);
void jimu_builtin_tool_destroy(JimuBuiltinTool *tool);

/* ---- Session Lifecycle & Operations ---- */

JimuSession *jimu_session_create(
    const char *config_json,
    JimuHttpCallbacks http_callbacks,
    JimuToolCallbacks tool_callbacks,
    char **out_error
);

void jimu_session_close(
    JimuSession *session,
    void *ctx,
    JimuVoidDoneFn on_done
);

void jimu_session_destroy(JimuSession *session);

JimuStatus jimu_session_prompt_start(
    JimuSession *session,
    const char *prompt_json,
    void *ctx,
    JimuEventFn on_event,
    JimuStreamDoneFn on_done,
    char **out_error
);

bool jimu_session_steer(
    JimuSession *session,
    const char *id,
    const char *text,
    bool is_meta
);

JimuStatus jimu_session_follow_up(
    JimuSession *session,
    const char *id,
    const char *text,
    bool is_meta,
    char **out_error
);

void jimu_session_interrupt(
    JimuSession *session,
    void *ctx,
    JimuVoidDoneFn on_done
);

JimuStatus jimu_session_set_context(
    JimuSession *session,
    const char *name,
    const char *text,
    bool *out_changed,
    char **out_error
);

JimuStatus jimu_session_append_named_context(
    JimuSession *session,
    const char *name,
    const char *text,
    char **out_error
);

JimuStatus jimu_session_append_meta(
    JimuSession *session,
    const char *text,
    char **out_error
);

JimuStatus jimu_session_set_model(
    JimuSession *session,
    const char *model_json,
    char **out_error
);

JimuStatus jimu_session_set_thinking_level(
    JimuSession *session,
    const char *level_str,
    char **out_error
);

JimuStatus jimu_session_set_fast(
    JimuSession *session,
    bool fast,
    char **out_error
);

JimuStatus jimu_session_set_api_key(
    JimuSession *session,
    const char *api_key,
    char **out_error
);

JimuStatus jimu_session_upsert_tools(
    JimuSession *session,
    const char *tools_json,
    char **out_error
);

JimuStatus jimu_session_remove_tools(
    JimuSession *session,
    const char *names_json,
    size_t *out_removed,
    char **out_error
);

void jimu_session_compact(
    JimuSession *session,
    void *ctx,
    JimuResultDoneFn on_done
);

JimuStatus jimu_session_fork(
    JimuSession *session,
    const char *entry_id,
    char **out_error
);

JimuStatus jimu_session_entries(
    JimuSession *session,
    char **out_json,
    char **out_error
);

JimuStatus jimu_session_info(
    JimuSession *session,
    char **out_json,
    char **out_error
);

JimuStatus jimu_session_reload_extensions(
    JimuSession *session,
    char **out_error
);

JimuStatus jimu_session_reload_skills(
    JimuSession *session,
    bool *out_changed,
    char **out_error
);

JimuStatus jimu_session_reload_instructions(
    JimuSession *session,
    bool *out_changed,
    char **out_error
);

/* Returns the instruction catalog as JSON. Free the returned string with jimu_string_free. */
char *jimu_discover_instructions(const char *config_json, char **out_error);

/* ---- Skill Discovery ---- */

char *jimu_discover_skills(
    const char *roots_json,
    char **out_error
);

char *jimu_discover_skills_with_config(const char *config_json, char **out_error);

#ifdef __cplusplus
}
#endif

#endif /* JIMU_H */
