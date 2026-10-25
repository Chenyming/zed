---
title: Parallel Agents - Zed
description: Run multiple agent threads and Terminal Threads concurrently using the Threads Sidebar, manage them across projects, and isolate work using Git worktrees.
---

# Parallel Agents

Parallel Agents lets you run multiple agent threads and Terminal Threads at once from the Threads Sidebar. Each thread works independently with its own agent, context window, and conversation history. Terminal Threads appear alongside agent threads in the same sidebar, so you can switch between them without leaving the Agent Panel.

Open the Threads Sidebar with {#kb multi_workspace::ToggleWorkspaceSidebar}.

Use **Panel Layout > Agentic** from the user menu in the title bar (or the {#action workspace::UseAgenticLayout} action) to place the Agent Panel and Threads Sidebar on the left, with the Project Panel, Git Panel, and other panels on the right. Use **Panel Layout > Classic** (or {#action workspace::UseClassicLayout}) to restore the editor-oriented layout. You can still rearrange individual panels by right-clicking any panel icon.

## Threads Sidebar {#threads-sidebar}

The sidebar shows your threads grouped by project. Each project gets its own section with a header. Threads appear below with their title, status indicator, and which agent is running them. Threads running in linked Git worktrees appear under the same project as their main worktree. See [Worktree Isolation](#worktree-isolation).

Terminal Threads also appear as entries in the sidebar alongside agent threads, identified by a terminal icon. Click one to switch to it.

To focus the sidebar without toggling it, use {#kb multi_workspace::FocusWorkspaceSidebar}. To search your threads, press {#kb agents_sidebar::FocusSidebarFilter} while the sidebar is focused.

To change the sidebar’s position, open the Settings Editor and search for “Threads Sidebar Position”, which writes `agent.threads_sidebar.position`. To change its default width, search for “Threads Sidebar Default Width”. See [`agent.threads_sidebar.default_width`](../reference/all-settings.md#agent-threads-sidebar-default-width) for values and a configuration example.

Drag the divider to resize the sidebar. Changing the default width updates even a manually resized sidebar. Double-click the divider to return to the configured width.

The “Threads Sidebar Auto Open” setting controls whether opening a folder in an existing window opens the sidebar. Its JSON key is `agent.threads_sidebar.auto_open`.

### Switching Threads {#switching-threads}

Click any thread in the sidebar to switch to it. The Agent Panel updates to show that thread's conversation.

For quick switching without opening the sidebar, use the thread switcher: press {#kb agents_sidebar::ToggleThreadSwitcher} to cycle forward through recent threads, or hold `Shift` while pressing that binding to go backward. This works from both the Agent Panel and the Threads Sidebar.

### Thread History {#threads-history}

To delete a thread, hover over it and click the trash icon, or select it and press {#kb agent::RemoveSelectedThread}. Stop a running thread before deleting it.

The Thread History view holds all your remaining threads, including threads saved by earlier versions of Zed. Toggle it with {#kb agents_sidebar::ToggleThreadHistory} or by clicking the clock icon in the sidebar bottom bar, next to the sidebar toggle.

To open a thread, click it in Thread History. Zed opens it in the Agent Panel. Worktrees archived by earlier versions of Zed can still be restored when you open their threads.

You can also delete threads from Thread History using the trash icon. Deletion removes the conversation records and its sandbox temporary files, while preserving your project files, worktrees, and Git branches. For external agents, Zed requests deletion from the agent on the machine that owns the session. If the agent does not support deletion, Zed removes its own record and tells you that the agent's history remains. If the request fails, you can retry, delete only Zed's record, or cancel. Locally deleted sessions are excluded from future history imports, including after restarting Zed. Deleted threads cannot be recovered from Thread History.

You can search your threads in history; search will fuzzy match on thread titles.

### Importing External Agent Threads {#importing-threads}

If you have External Agents installed, Zed will detect whether you have existing threads and invite you to import them into Zed. Once you open Thread History, you'll find an import icon button in the Thread History toolbar that lets you import threads at any time. Clicking on it opens a modal where you can select the agents whose threads you want to import.

> **Note:** Thread import is subject to agent support. Some agents (such as Cursor and Gemini CLI) are not currently supported.

## Running Multiple Threads {#running-multiple-threads}

Each thread runs independently, so you can send a prompt, open a second thread, and give it a different task while the first continues working. To scope a new thread to a specific project, hover over that project's header in the Threads Sidebar and click the `+` button, or use {#action agents_sidebar::NewThreadInGroup} from the keyboard. See [Creating New Threads](./agent-panel.md#new-thread) for the other entry points.

Each thread can use a different agent, so you can run Zed's built-in agent in one thread and an [External Agent](./external-agents.md) like Claude Code or Codex in another.

### Thread Types {#thread-types}

The Threads Sidebar can hold different thread types:

| Thread type                                   | Configuration                                                                          |
| --------------------------------------------- | -------------------------------------------------------------------------------------- |
| [Zed Agent thread](./zed-agent.md)            | Uses Zed Agent settings, profiles, tools, [Skills](./skills.md), Instructions, and MCP |
| [External Agent thread](./external-agents.md) | Uses the ACP integration and the agent's native configuration                          |
| [Terminal Thread](./terminal-threads.md)      | Runs a CLI/TUI in a terminal-backed thread; the CLI owns auth and configuration        |

## Multiple Projects {#multiple-projects}

The Threads Sidebar can hold multiple projects at once. Each project gets its own group with its own threads and conversation history. This mirrors how Zed handles projects in general — see [Windows & Projects](../windows-and-projects.md) for more on how projects open and how to manage them.

To add another project to the sidebar, click the **Add Project** button (open-folder icon) in the sidebar bottom bar. The popover that opens lists your recent projects and also provides **Add Local Folders** and **Add Remote Folder** buttons at the bottom.

### Multi-Root Folder Projects {#multi-root-folder-projects}

A single project can contain multiple folders (a multi-root folder project). Agents can then read and write across all of those folders in a single thread. There are multiple ways to set one up:

- **From the sidebar:** Click the **Add Project** button, choose **Add Local Folders**, and select multiple folders in the file picker. They open together as one multi-root project.
- **From the title bar:** Click the project picker (the leftmost project name). For any local entry in the recent projects list, hover it and click the folder-with-plus icon (**Add Folder to this Project**) to merge that project's folders into the current project.
- **From the Project panel:** Right-click a root folder or any empty space in the Project panel and choose **Add Folders to Project** to add more folders to the current project.

## Worktree Isolation {#worktree-isolation}

If two threads might edit the same files, start one in a new [Git worktree](../git.md#git-worktrees) to give it an isolated checkout.

Worktrees are managed from the title bar. Click the worktree picker (to the right of the project picker) to switch between existing worktrees or create a new one. New worktrees are created in a detached HEAD state, so you won't accidentally share a branch between worktrees.

Once you're in a new worktree, use the branch picker next to the worktree picker to create a new branch or check out an existing one. If the branch you pick is already checked out in another worktree, the current worktree stays in detached HEAD until you choose a different branch.

To automate setup steps whenever a new worktree is created, use a [Task hook](../tasks.md#hooks). The `create_worktree` hook runs automatically after Zed creates a linked worktree, with `ZED_WORKTREE_ROOT` pointing at the new worktree and `ZED_MAIN_GIT_WORKTREE` pointing at the original repository.

After the agent finishes, review the diff and merge the changes through your normal Git workflow. If the thread was running in a linked worktree and no other active threads use it, moving the thread to Thread History saves the worktree's Git state and removes it from disk. Restoring the thread from history restores the worktree.

## See Also {#see-also}

- [Agent Panel](./agent-panel.md): Manage individual threads and configure the agent
- [External Agents](./external-agents.md): Use ACP-integrated External Agents
- [Terminal Threads](./terminal-threads.md): Run agent CLIs and TUIs directly in Zed
- [Tools](./tools.md): Built-in tools available in each thread
