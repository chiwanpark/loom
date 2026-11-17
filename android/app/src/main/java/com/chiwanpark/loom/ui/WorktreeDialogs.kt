package com.chiwanpark.loom.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Checkbox
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.chiwanpark.loom.core.Worktree

/** Asks for the branch and the base of a new session's worktree; empty fields take the defaults. */
@Composable
fun WorktreeSessionDialog(onCreate: (branch: String, base: String) -> Unit, onDismiss: () -> Unit) {
    var branch by remember { mutableStateOf("") }
    var base by remember { mutableStateOf("") }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("New session in a worktree") },
        text = {
            Column(Modifier.verticalScroll(rememberScrollState()), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(
                    "The session works in a new git worktree on a branch of its own, outside the workspace. " +
                        "When it is done, merge the branch or open a pull request from the session's menu.",
                    style = MaterialTheme.typography.bodyMedium,
                )
                OutlinedTextField(
                    value = branch,
                    onValueChange = { branch = it },
                    label = { Text("Branch") },
                    placeholder = { Text("loom/<session>") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                OutlinedTextField(
                    value = base,
                    onValueChange = { base = it },
                    label = { Text("Base") },
                    placeholder = { Text("The branch of the workspace") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
            }
        },
        confirmButton = {
            TextButton(onClick = {
                onDismiss()
                onCreate(branch.trim(), base.trim())
            }) { Text("Create") }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

/** Merges the branch of a worktree into its base: with a message, as one squashed commit; without, with a merge commit. */
@Composable
fun MergeWorktreeDialog(
    worktree: Worktree,
    title: String,
    onMerge: (squashMessage: String?) -> Unit,
    onDismiss: () -> Unit,
) {
    var squash by remember { mutableStateOf(true) }
    var message by remember { mutableStateOf(title) }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Merge ${worktree.branch}?") },
        text = {
            Column(Modifier.verticalScroll(rememberScrollState()), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(
                    "The agent merges ${worktree.branch} into ${worktree.base} in the workspace, which must have " +
                        "${worktree.base} checked out and no uncommitted changes. The worktree stays.",
                    style = MaterialTheme.typography.bodyMedium,
                )
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Checkbox(checked = squash, onCheckedChange = { squash = it })
                    Text("Squash into one commit")
                }
                if (squash) {
                    OutlinedTextField(
                        value = message,
                        onValueChange = { message = it },
                        label = { Text("Commit message") },
                        modifier = Modifier.fillMaxWidth(),
                    )
                }
            }
        },
        confirmButton = {
            TextButton(
                onClick = {
                    onDismiss()
                    onMerge(if (squash) message.trim() else null)
                },
                enabled = !squash || message.isNotBlank(),
            ) { Text("Merge") }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

/** Pushes the branch of a worktree and opens a pull request into its base with `gh` on the agent's host. */
@Composable
fun PullRequestDialog(
    worktree: Worktree,
    title: String,
    onOpen: (title: String, body: String, draft: Boolean) -> Unit,
    onDismiss: () -> Unit,
) {
    var name by remember { mutableStateOf(title) }
    var body by remember { mutableStateOf("") }
    var draft by remember { mutableStateOf(false) }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Open a pull request") },
        text = {
            Column(Modifier.verticalScroll(rememberScrollState()), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(
                    "The agent pushes ${worktree.branch} and opens a pull request into ${worktree.base} " +
                        "with the GitHub CLI (gh) on its host.",
                    style = MaterialTheme.typography.bodyMedium,
                )
                OutlinedTextField(
                    value = name,
                    onValueChange = { name = it },
                    label = { Text("Title") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                OutlinedTextField(
                    value = body,
                    onValueChange = { body = it },
                    label = { Text("Description") },
                    minLines = 3,
                    modifier = Modifier.fillMaxWidth(),
                )
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Checkbox(checked = draft, onCheckedChange = { draft = it })
                    Text("Draft")
                }
            }
        },
        confirmButton = {
            TextButton(
                onClick = {
                    onDismiss()
                    onOpen(name.trim(), body, draft)
                },
                enabled = name.isNotBlank(),
            ) { Text("Open") }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}
