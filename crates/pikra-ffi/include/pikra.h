#ifndef PIKRA_H
#define PIKRA_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct PikraSession PikraSession;
typedef struct PikraHttpStream PikraHttpStream;
typedef struct PikraToolCompletion PikraToolCompletion;

typedef enum PikraStatus {
    PIKRA_OK = 0,
    PIKRA_ERR_INVALID_ARGUMENT = 1,
    PIKRA_ERR_BUSY = 2,
    PIKRA_ERR_CLOSED = 3,
    PIKRA_ERR_NOT_FOUND = 4,
    PIKRA_ERR_IO = 5,
    PIKRA_ERR_INTERNAL = 6,
    PIKRA_ERR_CANCELLED = 7,
    PIKRA_ERR_BACKPRESSURE = 8
} PikraStatus;

typedef enum PikraHttpErrorKind {
    PIKRA_HTTP_OK = 0,
    PIKRA_HTTP_ERR_ABORTED = 1,
    PIKRA_HTTP_ERR_TIMEOUT = 2,
    PIKRA_HTTP_ERR_NETWORK = 3
} PikraHttpErrorKind;

/* ---- Host Streaming HTTP Transport Callbacks ---- */

typedef void (*PikraHttpStartFn)(
    void *ctx,
    uint64_t request_id,
    const char *request_json,
    const uint8_t *body_ptr,
    size_t body_len,
    PikraHttpStream *stream
);

typedef void (*PikraHttpCancelFn)(
    void *ctx,
    uint64_t request_id
);

typedef void (*PikraVoidDoneFn)(
    void *ctx
);

typedef struct PikraHttpCallbacks {
    void *ctx;
    PikraHttpStartFn start;
    PikraHttpCancelFn cancel;
    PikraVoidDoneFn destroy;
} PikraHttpCallbacks;

/* ---- Host Native Async Tool Callbacks ---- */

typedef void (*PikraToolExecuteFn)(
    void *ctx,
    uint64_t call_handle_id,
    const char *tool_call_id,
    const char *tool_name,
    const char *args_json,
    PikraToolCompletion *completion
);

typedef void (*PikraToolCancelFn)(
    void *ctx,
    uint64_t call_handle_id
);

typedef struct PikraToolCallbacks {
    void *ctx;
    PikraToolExecuteFn execute;
    PikraToolCancelFn cancel;
} PikraToolCallbacks;

/* ---- Event & Async Completion Callbacks ---- */

typedef void (*PikraEventFn)(
    void *ctx,
    const char *event_json
);

typedef void (*PikraStreamDoneFn)(
    void *ctx,
    PikraStatus status,
    const char *error_message
);

typedef void (*PikraResultDoneFn)(
    void *ctx,
    PikraStatus status,
    const char *result_json,
    const char *error_message
);

/* ---- Memory Management ---- */

void pikra_string_free(char *ptr);

/* ---- HTTP Stream Sink Functions (called by host) ---- */

PikraStatus pikra_http_on_response(
    PikraHttpStream *stream,
    uint16_t status_code,
    const char *headers_json
);

PikraStatus pikra_http_push_chunk(
    PikraHttpStream *stream,
    const uint8_t *data,
    size_t len
);

void pikra_http_finish(
    PikraHttpStream *stream,
    int32_t error_kind,
    const char *error_message
);

/* ---- Tool Completion Functions (called by host) ---- */

PikraStatus pikra_tool_update(
    PikraToolCompletion *completion,
    const char *result_json
);

void pikra_tool_complete(
    PikraToolCompletion *completion,
    const char *result_json,
    const char *error_message
);

/* ---- Independent Workspace Tools ---- */

typedef struct PikraBuiltinEnv PikraBuiltinEnv;
typedef struct PikraBuiltinTool PikraBuiltinTool;
typedef struct PikraBuiltinRun PikraBuiltinRun;
typedef struct PikraBuiltinEnvCompletion PikraBuiltinEnvCompletion;

typedef void (*PikraBuiltinEnvProviderFn)(void *ctx, PikraBuiltinEnvCompletion *completion);

typedef struct PikraBuiltinCallbacks {
    void *ctx;
    PikraBuiltinEnvProviderFn environment;
    PikraEventFn background_event;
    PikraVoidDoneFn destroy;
} PikraBuiltinCallbacks;

/* Config JSON takes cwd, optional max_output_bytes and default_bash_timeout_ms.
 * Either callbacks table may be NULL. On success the environment owns the callback
 * contexts until their destroy functions run. On failure ownership stays with the caller.
 * Background callbacks run on worker threads and must enqueue and return promptly. */
PikraBuiltinEnv *pikra_builtin_env_create(
    const char *config_json,
    const PikraBuiltinCallbacks *callbacks,
    const PikraHttpCallbacks *http_callbacks,
    char **out_error
);

/* Complete exactly once per environment request. Pass a JSON object of strings or NULL. */
void pikra_builtin_env_complete(PikraBuiltinEnvCompletion *completion, const char *environment_json);

/* Shutdown cancels active calls, reaps background processes and then calls on_done.
 * Destroy starts the same shutdown and frees the handle without waiting. */
void pikra_builtin_env_shutdown(PikraBuiltinEnv *env, void *ctx, PikraVoidDoneFn on_done);
void pikra_builtin_env_destroy(PikraBuiltinEnv *env);

PikraBuiltinTool *pikra_builtin_tool_read(PikraBuiltinEnv *env, char **out_error);
PikraBuiltinTool *pikra_builtin_tool_write(PikraBuiltinEnv *env, char **out_error);
PikraBuiltinTool *pikra_builtin_tool_edit(PikraBuiltinEnv *env, char **out_error);
PikraBuiltinTool *pikra_builtin_tool_apply_patch(PikraBuiltinEnv *env, char **out_error);
PikraBuiltinTool *pikra_builtin_tool_find(PikraBuiltinEnv *env, char **out_error);
PikraBuiltinTool *pikra_builtin_tool_grep(PikraBuiltinEnv *env, char **out_error);
PikraBuiltinTool *pikra_builtin_tool_sleep(PikraBuiltinEnv *env, char **out_error);
PikraBuiltinTool *pikra_builtin_tool_web_fetch(PikraBuiltinEnv *env, char **out_error);

/* Subprocess constructors report an invalid argument on iOS. */
PikraBuiltinTool *pikra_builtin_tool_bash(PikraBuiltinEnv *env, char **out_error);
PikraBuiltinTool *pikra_builtin_tool_exec_command(PikraBuiltinEnv *env, char **out_error);
PikraBuiltinTool *pikra_builtin_tool_write_stdin(PikraBuiltinEnv *env, char **out_error);
PikraBuiltinTool *pikra_builtin_tool_monitor(PikraBuiltinEnv *env, char **out_error);

PikraBuiltinTool *pikra_builtin_tool_override(
    const PikraBuiltinTool *tool, const char *override_json, char **out_error
);
char *pikra_builtin_tool_definition_json(const PikraBuiltinTool *tool);
bool pikra_builtin_tool_is_sequential(const PikraBuiltinTool *tool);

/* A successful start calls done exactly once. The caller owns the run handle and
 * must destroy it, independently of tool lifetime. Progress and result JSON are
 * borrowed for the duration of each callback. */
PikraBuiltinRun *pikra_builtin_tool_execute(
    const PikraBuiltinTool *tool,
    const char *call_id,
    const char *arguments_json,
    void *ctx,
    PikraEventFn progress,
    PikraResultDoneFn done,
    char **out_error
);
void pikra_builtin_tool_cancel(const PikraBuiltinRun *run);
void pikra_builtin_run_destroy(PikraBuiltinRun *run);
void pikra_builtin_tool_destroy(PikraBuiltinTool *tool);

/* ---- Session Lifecycle & Operations ---- */

PikraSession *pikra_session_create(
    const char *config_json,
    PikraHttpCallbacks http_callbacks,
    PikraToolCallbacks tool_callbacks,
    char **out_error
);

void pikra_session_close(
    PikraSession *session,
    void *ctx,
    PikraVoidDoneFn on_done
);

void pikra_session_destroy(PikraSession *session);

PikraStatus pikra_session_prompt_start(
    PikraSession *session,
    const char *prompt_json,
    void *ctx,
    PikraEventFn on_event,
    PikraStreamDoneFn on_done,
    char **out_error
);

bool pikra_session_steer(
    PikraSession *session,
    const char *id,
    const char *text
);

PikraStatus pikra_session_follow_up(
    PikraSession *session,
    const char *id,
    const char *text,
    char **out_error
);

void pikra_session_interrupt(
    PikraSession *session,
    void *ctx,
    PikraVoidDoneFn on_done
);

PikraStatus pikra_session_set_context(
    PikraSession *session,
    const char *name,
    const char *text,
    bool *out_changed,
    char **out_error
);

PikraStatus pikra_session_append_context(
    PikraSession *session,
    const char *text,
    char **out_error
);

PikraStatus pikra_session_set_model(
    PikraSession *session,
    const char *model_json,
    char **out_error
);

PikraStatus pikra_session_set_thinking_level(
    PikraSession *session,
    const char *level_str,
    char **out_error
);

PikraStatus pikra_session_set_fast(
    PikraSession *session,
    bool fast,
    char **out_error
);

PikraStatus pikra_session_set_api_key(
    PikraSession *session,
    const char *api_key,
    char **out_error
);

PikraStatus pikra_session_upsert_tools(
    PikraSession *session,
    const char *tools_json,
    char **out_error
);

PikraStatus pikra_session_remove_tools(
    PikraSession *session,
    const char *names_json,
    size_t *out_removed,
    char **out_error
);

void pikra_session_compact(
    PikraSession *session,
    void *ctx,
    PikraResultDoneFn on_done
);

PikraStatus pikra_session_fork(
    PikraSession *session,
    const char *entry_id,
    char **out_error
);

PikraStatus pikra_session_entries(
    PikraSession *session,
    char **out_json,
    char **out_error
);

PikraStatus pikra_session_info(
    PikraSession *session,
    char **out_json,
    char **out_error
);

PikraStatus pikra_session_reload_extensions(
    PikraSession *session,
    char **out_error
);

PikraStatus pikra_session_reload_skills(
    PikraSession *session,
    bool *out_changed,
    char **out_error
);

PikraStatus pikra_session_reload_instructions(
    PikraSession *session,
    bool *out_changed,
    char **out_error
);

/* Returns the instruction catalog as JSON. Free the returned string with pikra_string_free. */
char *pikra_discover_instructions(const char *config_json, char **out_error);

/* ---- Skill Discovery ---- */

char *pikra_discover_skills(
    const char *roots_json,
    char **out_error
);

char *pikra_discover_skills_with_config(const char *config_json, char **out_error);

#ifdef __cplusplus
}
#endif

#endif /* PIKRA_H */
