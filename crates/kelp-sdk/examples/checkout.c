/*
 * Checks a file out the way an editor's "Check Out" button would:
 *
 *     checkout <workspace> <kelp home> <file>
 *
 * Prints the file's state before and after opening it for edit.
 */

#include <stdio.h>

#include "kelp.h"

static const char *state_name(KelpFileState s) {
    switch (s) {
    case KELP_FILE_SYNCED: return "synced";
    case KELP_FILE_MODIFIED: return "modified";
    case KELP_FILE_OPENED_FOR_EDIT: return "opened-for-edit";
    case KELP_FILE_OPENED_FOR_ADD: return "opened-for-add";
    case KELP_FILE_OPENED_FOR_DELETE: return "opened-for-delete";
    case KELP_FILE_UNTRACKED: return "untracked";
    case KELP_FILE_MISSING: return "missing";
    default: return "unknown";
    }
}

static int show(const KelpWorkspace *ws, const char *path) {
    KelpFileInfo info;
    if (kelp_file_info(ws, path, &info) != KELP_OK) {
        fprintf(stderr, "file_info: %s\n", kelp_last_error());
        return 1;
    }
    printf("%s state=%s exclusive=%d locked_here=%d locked_by=%s\n", path, state_name(info.state),
           info.exclusive, info.locked_here, info.locked_by);
    return 0;
}

int main(int argc, char **argv) {
    if (argc != 4) {
        fprintf(stderr, "usage: checkout <workspace> <kelp home> <file>\n");
        return 2;
    }
    if (kelp_abi_version() != KELP_ABI_VERSION) {
        fprintf(stderr, "ABI mismatch\n");
        return 2;
    }
    KelpWorkspace *ws = kelp_workspace_open(argv[1], argv[2]);
    if (!ws) {
        fprintf(stderr, "open: %s\n", kelp_last_error());
        return 1;
    }
    const char *paths[] = {argv[3]};
    int failed = show(ws, argv[3]);
    if (!failed && kelp_edit(ws, paths, 1, 0) != KELP_OK) {
        fprintf(stderr, "edit: %s\n", kelp_last_error());
        failed = 1;
    }
    if (!failed) {
        failed = show(ws, argv[3]);
    }
    kelp_workspace_close(ws);
    return failed;
}
