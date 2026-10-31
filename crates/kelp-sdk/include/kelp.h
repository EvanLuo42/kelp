/*
 * Kelp C SDK.
 *
 * Lets engines, editors and DCC tools query and change a Kelp workspace.
 * Operations go to kelpd when it is running and otherwise run inside the
 * calling process. Every function may be called from any thread; calls on one
 * workspace handle are serialized.
 *
 * Functions returning int return KELP_OK or KELP_ERROR. After an error,
 * kelp_last_error() describes it until the next call on the same thread.
 * Strings are NUL-terminated UTF-8. File paths are absolute or relative to
 * the workspace root.
 */

#ifndef KELP_H
#define KELP_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define KELP_ABI_VERSION 1
#define KELP_OK 0
#define KELP_ERROR 1

typedef struct KelpWorkspace KelpWorkspace;

typedef enum KelpFileState {
    KELP_FILE_UNKNOWN = 0,          /* neither in the workspace nor on disk */
    KELP_FILE_UNTRACKED = 1,        /* on disk but not in the workspace */
    KELP_FILE_SYNCED = 2,           /* matches what the workspace has */
    KELP_FILE_MODIFIED = 3,         /* changed on disk without being opened */
    KELP_FILE_MISSING = 4,          /* missing without being opened for delete */
    KELP_FILE_OPENED_FOR_ADD = 5,
    KELP_FILE_OPENED_FOR_EDIT = 6,
    KELP_FILE_OPENED_FOR_DELETE = 7
} KelpFileState;

typedef struct KelpFileInfo {
    KelpFileState state;
    uint32_t changelist;   /* pending changelist the file is opened in; 0 is the default */
    bool needs_resolve;    /* must be resolved before submitting */
    bool exclusive;        /* the typemap requires a lock to edit it */
    bool locked;           /* someone holds its lock */
    bool locked_here;      /* this workspace holds its lock */
    bool out_of_date;      /* the stream has a newer revision */
    char locked_by[128];   /* "user@workspace" of the lock holder, or empty */
} KelpFileInfo;

/* Returns KELP_ABI_VERSION of the loaded library. */
uint32_t kelp_abi_version(void);

/* Returns the library version as a static string. */
const char *kelp_version(void);

/* Returns the last error on this thread. */
const char *kelp_last_error(void);

/* Opens the workspace containing `path`. `home` is the Kelp home directory,
 * or NULL for KELP_HOME or the platform default. Returns NULL on error. */
KelpWorkspace *kelp_workspace_open(const char *path, const char *home);

/* Closes a handle. NULL is ignored. */
void kelp_workspace_close(KelpWorkspace *ws);

/* Fills `out` with the state of one file, asking the server about locks and
 * newer revisions. */
int kelp_file_info(const KelpWorkspace *ws, const char *path, KelpFileInfo *out);

/* Opens files for edit in `changelist`, locking exclusive files. */
int kelp_edit(const KelpWorkspace *ws, const char *const *paths, size_t count, uint32_t changelist);

/* Opens new files for add in `changelist`. */
int kelp_add(const KelpWorkspace *ws, const char *const *paths, size_t count, uint32_t changelist);

/* Opens files for delete in `changelist` and removes them from disk. */
int kelp_delete(const KelpWorkspace *ws, const char *const *paths, size_t count, uint32_t changelist);

/* Reverts opened files. */
int kelp_revert(const KelpWorkspace *ws, const char *const *paths, size_t count);

/* Syncs to the head of the stream; stores the changelist in `out_cl` if not NULL. */
int kelp_sync(const KelpWorkspace *ws, uint64_t *out_cl);

/* Submits `changelist`. `description` may be NULL to keep the changelist's
 * own. Stores the submitted changelist number in `out_cl` if not NULL. */
int kelp_submit(const KelpWorkspace *ws, uint32_t changelist, const char *description, uint64_t *out_cl);

#ifdef __cplusplus
}
#endif

#endif /* KELP_H */
