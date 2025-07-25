use std::str::FromStr;
use std::sync::Arc;

use bstr::BString;
use but_core::{TreeChange, UnifiedDiff};
use but_graph::VirtualBranchesTomlMetadata;
use but_workspace::StackId;
use but_workspace::ui::StackEntry;
use gitbutler_command_context::CommandContext;
use gitbutler_oplog::{OplogExt, SnapshotExt};
use gitbutler_oxidize::ObjectIdExt;
use gitbutler_project::Project;
use gitbutler_stack::{PatchReferenceUpdate, VirtualBranchesHandle};
use schemars::{JsonSchema, schema_for};

use crate::emit::EmitStackUpdate;
use crate::tool::{Tool, ToolResult, Toolset, error_to_json, result_to_json};

/// Creates a toolset for any kind of workspace operations.
pub fn workspace_toolset<'a>(
    ctx: &'a mut CommandContext,
    app_handle: Option<&'a tauri::AppHandle>,
    message_id: String,
) -> anyhow::Result<Toolset<'a>> {
    let mut toolset = Toolset::new(ctx, app_handle, Some(message_id));

    toolset.register_tool(Commit);
    toolset.register_tool(CreateBranch);
    toolset.register_tool(Amend);
    toolset.register_tool(GetProjectStatus);
    toolset.register_tool(CreateBlankCommit);
    toolset.register_tool(MoveFileChanges);
    toolset.register_tool(GetCommitDetails);

    Ok(toolset)
}

/// Creates a toolset for workspace-related operations.
pub fn commit_toolset<'a>(
    ctx: &'a mut CommandContext,
    app_handle: Option<&'a tauri::AppHandle>,
) -> anyhow::Result<Toolset<'a>> {
    let mut toolset = Toolset::new(ctx, app_handle, None);

    toolset.register_tool(Commit);
    toolset.register_tool(CreateBranch);

    Ok(toolset)
}

/// Creates a toolset for amend operations.
pub fn amend_toolset<'a>(
    ctx: &'a mut CommandContext,
    app_handle: Option<&'a tauri::AppHandle>,
) -> anyhow::Result<Toolset<'a>> {
    let mut toolset = Toolset::new(ctx, app_handle, None);

    toolset.register_tool(Amend);
    toolset.register_tool(GetProjectStatus);

    Ok(toolset)
}

pub struct Commit;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommitParameters {
    /// The commit title.
    #[schemars(description = "
    <description>
        The commit message title.
        This is only a short summary of the commit.
    </description>

    <important_notes>
        The commit message title should be concise and descriptive.
        It is typically a single line that summarizes the changes made in the commit.
        For example: 'Fix issue with user login' or 'Update README with installation instructions'.
        Don't excede 50 characters in length.
    </important_notes>
    ")]
    pub message_title: String,
    /// The commit description.
    #[schemars(description = "
    <description>
        The commit message body.
        This is a more detailed description of the changes made in the commit.
    </description>

    <important_notes>
        The commit message body should provide context and details about the changes made.
        It should span multiple lines if necessary.
        A good description focuses on describing the 'what' of the changes.
        Don't make assumption about the 'why', only describe the changes in the context of the branch (and other commits if any).
    </important_notes>
    ")]
    pub message_body: String,
    /// The branch name to commit to.
    #[schemars(description = "
    <description>
        The name of the branch to commit to.
        If this is the name of a branch that does not exist, it will be created.
    </description>

    <important_notes>
        The branch name should be a valid Git branch name.
        It should not contain spaces or special characters.
        Keep it to maximum 5 words, and use hyphens to separate words.
        Don't use slashes or other special characters.
    </important_notes>
    ")]
    pub branch_name: String,
    /// The branch description.
    #[schemars(description = "
    <description>
        The description of the branch.
        This is a short summary of the branch's purpose.
        If the branch already exists, this will be overwritten with this description.
    </description>

    <important_notes>
        The branch description should be a concise summary of the branch's purpose and changes.
        It's important to keep it clear and informative.
        This description should also point out which kind of changes should be assigned to this branch.
    </important_notes>
    ")]
    pub branch_description: String,
    /// The list of files to commit.
    #[schemars(description = "
        <description>
            The list of file paths to commit.
        </description>

        <important_notes>
            The file paths should be relative to the workspace root.
        </important_notes>
        ")]
    pub files: Vec<String>,
}

/// Commit tool.
///
/// Takes in a commit message, target branch name, and a list of file paths to commit.
impl Tool for Commit {
    fn name(&self) -> String {
        "commit".to_string()
    }

    fn description(&self) -> String {
        "
        <description>
            Commit file changes to a branch in the workspace.
        </description>

        <important_notes>
            This tool allows you to commit changes to a specific branch in the workspace.
            You can specify the commit message, target branch name, and a list of file paths to commit.
            If the branch does not exist, it will be created.
        </important_notes>
        ".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        let schema = schema_for!(CommitParameters);
        serde_json::to_value(&schema).unwrap_or_default()
    }

    fn call(
        self: Arc<Self>,
        parameters: serde_json::Value,
        ctx: &mut CommandContext,
        app_handle: Option<&tauri::AppHandle>,
    ) -> anyhow::Result<serde_json::Value> {
        let params: CommitParameters = serde_json::from_value(parameters)
            .map_err(|e| anyhow::anyhow!("Failed to parse input parameters: {}", e))?;

        let value = create_commit(ctx, app_handle, params).to_json("create_commit");
        Ok(value)
    }
}

pub fn create_commit(
    ctx: &mut CommandContext,
    app_handle: Option<&tauri::AppHandle>,
    params: CommitParameters,
) -> Result<but_workspace::commit_engine::ui::CreateCommitOutcome, anyhow::Error> {
    let repo = ctx.gix_repo()?;
    let mut guard = ctx.project().exclusive_worktree_access();
    let worktree = but_core::diff::worktree_changes(&repo)?;
    let vb_state = VirtualBranchesHandle::new(ctx.project().gb_dir());

    let file_changes: Vec<but_workspace::DiffSpec> = worktree
        .changes
        .iter()
        .filter(|change| params.files.contains(&change.path.to_string()))
        .map(Into::into)
        .collect::<Vec<_>>();

    let stacks = stacks(ctx, &repo)?;

    let stack_id = stacks
        .iter()
        .find_map(|s| {
            let found = s.heads.iter().any(|h| h.name == params.branch_name);
            if found { Some(s.id) } else { None }
        })
        .unwrap_or_else(|| {
            let perm = guard.write_permission();

            let branch = gitbutler_branch::BranchCreateRequest {
                name: Some(params.branch_name.clone()),
                ..Default::default()
            };

            let stack = gitbutler_branch_actions::create_virtual_branch(ctx, &branch, perm)
                .expect("Failed to create virtual branch");
            stack.id
        });

    // Update the branch description.
    let mut stack = vb_state.get_stack(stack_id)?;
    stack.update_branch(
        ctx,
        params.branch_name.clone(),
        &PatchReferenceUpdate {
            description: Some(Some(params.branch_description)),
            ..Default::default()
        },
    )?;

    let snapshot_tree = ctx.prepare_snapshot(guard.read_permission());

    let message = format!(
        "{}\n\n{}",
        params.message_title.trim(),
        params.message_body.trim()
    );

    let outcome = but_workspace::commit_engine::create_commit_simple(
        ctx,
        stack_id,
        None,
        file_changes,
        message.clone(),
        params.branch_name.clone(),
        guard.write_permission(),
    );

    let _ = snapshot_tree.and_then(|snapshot_tree| {
        ctx.snapshot_commit_creation(
            snapshot_tree,
            outcome.as_ref().err(),
            message.clone(),
            None,
            guard.write_permission(),
        )
    });

    // If there's an app handle provided, emit an event to update the stack details in the UI.
    if let Some(app_handle) = app_handle {
        let project_id = ctx.project().id;
        app_handle.emit_stack_update(project_id, stack_id);
    }

    let outcome: but_workspace::commit_engine::ui::CreateCommitOutcome = outcome?.into();
    Ok(outcome)
}

fn stacks(
    ctx: &CommandContext,
    repo: &gix::Repository,
) -> anyhow::Result<Vec<but_workspace::ui::StackEntry>> {
    let project = ctx.project();
    if ctx.app_settings().feature_flags.ws3 {
        let meta = ref_metadata_toml(ctx.project())?;
        but_workspace::stacks_v3(repo, &meta, but_workspace::StacksFilter::InWorkspace)
    } else {
        but_workspace::stacks(
            ctx,
            &project.gb_dir(),
            repo,
            but_workspace::StacksFilter::InWorkspace,
        )
    }
}

pub struct CreateBranch;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateBranchParameters {
    /// The name of the branch to create.
    #[schemars(description = "
    <description>
        The name of the branch to create.
        If this is the name of a branch that does not exist, it will be created.
    </description>

    <important_notes>
        The branch name should be a valid Git branch name.
        It should not contain spaces or special characters.
        Keep it to maximum 5 words, and use hyphens to separate words.
        Don't use slashes or other special characters.
    </important_notes>
    ")]
    pub branch_name: String,
    /// The branch description.
    #[schemars(description = "
    <description>
        The description of the branch.
        This is a short summary of the branch's purpose.
    </description>

    <important_notes>
        The branch description should be a concise summary of the branch's purpose and changes.
        It's important to keep it clear and informative.
        This description should also point out which kind of changes should be assigned to this branch.
    </important_notes>
    ")]
    pub branch_description: String,
}

impl Tool for CreateBranch {
    fn name(&self) -> String {
        "create_branch".to_string()
    }

    fn description(&self) -> String {
        "
        <description>
            Create a new branch in the workspace.
        </description>
        "
        .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        let schema = schema_for!(CreateBranchParameters);
        serde_json::to_value(&schema).unwrap_or_default()
    }

    fn call(
        self: Arc<Self>,
        parameters: serde_json::Value,
        ctx: &mut CommandContext,
        app_handle: Option<&tauri::AppHandle>,
    ) -> anyhow::Result<serde_json::Value> {
        let params: CreateBranchParameters = serde_json::from_value(parameters)
            .map_err(|e| anyhow::anyhow!("Failed to parse input parameters: {}", e))?;

        let stack = create_branch(ctx, app_handle, params).to_json("create branch");
        Ok(stack)
    }
}

pub fn create_branch(
    ctx: &mut CommandContext,
    app_handle: Option<&tauri::AppHandle>,
    params: CreateBranchParameters,
) -> Result<StackEntry, anyhow::Error> {
    let mut guard = ctx.project().exclusive_worktree_access();
    let perm = guard.write_permission();
    let vb_state = VirtualBranchesHandle::new(ctx.project().gb_dir());

    let name = params.branch_name;
    let description = params.branch_description;

    let branch = gitbutler_branch::BranchCreateRequest {
        name: Some(name.clone()),
        ..Default::default()
    };

    let stack_entry = gitbutler_branch_actions::create_virtual_branch(ctx, &branch, perm)?;

    // Update the branch description.
    let mut stack = vb_state.get_stack(stack_entry.id)?;
    stack.update_branch(
        ctx,
        name,
        &PatchReferenceUpdate {
            description: Some(Some(description)),
            ..Default::default()
        },
    )?;

    // If there's an app handle provided, emit an event to update the stack details in the UI.
    if let Some(app_handle) = app_handle {
        let project_id = ctx.project().id;
        app_handle.emit_stack_update(project_id, stack.id);
    }

    Ok(stack_entry)
}

pub struct Amend;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AmendParameters {
    /// The commit id to amend.
    #[schemars(description = "
    <description>
        The commit id of the commit to amend.
        This should be the id of the commit you want to modify.
    </description>

    <important_notes>
        The commit id should refer to a commit on the specified branch.
    </important_notes>
    ")]
    pub commit_id: String,
    /// The new commit title.
    #[schemars(description = "
    <description>
        The new commit message title.
        This is only a short summary of the commit.
    </description>

    <important_notes>
        The commit message title should be concise and descriptive.
        It is typically a single line that summarizes the changes made in the commit.
        For example: 'Fix issue with user login' or 'Update README with installation instructions'.
        Don't exceed 50 characters in length.
    </important_notes>
    ")]
    pub message_title: String,
    /// The new commit description.
    #[schemars(description = "
    <description>
        The new commit message body.
        This is a more detailed description of the changes made in the commit.
    </description>

    <important_notes>
        This should be an update of the existin commit message body in order to accomodate the changes amended into it.
        If the description already matches the changes, you can pass in the same description.
        The commit message body should provide context and details about the changes made.
        It should span multiple lines if necessary.
        A good description focuses on describing the 'what' of the changes.
        Don't make assumption about the 'why', only describe the changes in the context of the branch (and other commits if any).
    </important_notes>
    ")]
    pub message_body: String,
    /// The id of the stack to amend the commit on.
    #[schemars(description = "
    <description>
        This is the Id of the stack that contains the commit to amend.
    </description>

    <important_notes>
        The ID should refer to a stack in the workspace.
    </important_notes>
    ")]
    pub stack_id: String,
    /// The list of files to include in the amended commit.
    #[schemars(description = "
        <description>
            The list of file paths to include in the amended commit.
        </description>

        <important_notes>
            The file paths should be relative to the workspace root.
            Leave this empty if you only want to edit the commit message.
        </important_notes>
        ")]
    pub files: Vec<String>,
}

impl Tool for Amend {
    fn name(&self) -> String {
        "amend".to_string()
    }

    fn description(&self) -> String {
        "
        <description>
            Amend an existing commit on a branch in the workspace.
        </description>

        <important_notes>
            This tool allows you to amend a specific commit on a branch in the workspace.
            You can specify the new commit message, target branch name, commit id, and a list of file paths to include in the amended commit.
            Use this tool if:
            - You want to add uncommitted changes to an existing commit.
            - You want to update the commit message of an existing commit.
        </important_notes>
        ".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        let schema = schema_for!(AmendParameters);
        serde_json::to_value(&schema).unwrap_or_default()
    }

    fn call(
        self: Arc<Self>,
        parameters: serde_json::Value,
        ctx: &mut CommandContext,
        app_handle: Option<&tauri::AppHandle>,
    ) -> anyhow::Result<serde_json::Value> {
        let params: AmendParameters = serde_json::from_value(parameters)
            .map_err(|e| anyhow::anyhow!("Failed to parse input parameters: {}", e))?;

        let value = amend_commit(ctx, app_handle, params).to_json("amend_commit");
        Ok(value)
    }
}

pub fn amend_commit(
    ctx: &mut CommandContext,
    app_handle: Option<&tauri::AppHandle>,
    params: AmendParameters,
) -> Result<but_workspace::commit_engine::ui::CreateCommitOutcome, anyhow::Error> {
    let outcome = amend_commit_inner(ctx, app_handle, params)?;
    Ok(outcome.into())
}

pub fn amend_commit_inner(
    ctx: &mut CommandContext,
    app_handle: Option<&tauri::AppHandle>,
    params: AmendParameters,
) -> anyhow::Result<but_workspace::commit_engine::CreateCommitOutcome> {
    let repo = ctx.gix_repo()?;
    let project = ctx.project();
    let settings = ctx.app_settings();
    let mut guard = ctx.project().exclusive_worktree_access();
    let worktree = but_core::diff::worktree_changes(&repo)?;

    let file_changes: Vec<but_workspace::DiffSpec> = worktree
        .changes
        .iter()
        .filter(|change| params.files.contains(&change.path.to_string()))
        .map(Into::into)
        .collect::<Vec<_>>();

    let message = format!(
        "{}\n\n{}",
        params.message_title.trim(),
        params.message_body.trim()
    );

    let stack_id = StackId::from_str(&params.stack_id)?;

    let outcome = but_workspace::commit_engine::create_commit_and_update_refs_with_project(
        &repo,
        project,
        Some(stack_id),
        but_workspace::commit_engine::Destination::AmendCommit {
            commit_id: gix::ObjectId::from_str(&params.commit_id)?,
            new_message: Some(message),
        },
        None,
        file_changes,
        settings.context_lines,
        guard.write_permission(),
    );

    // If there's an app handle provided, emit an event to update the stack details in the UI.
    if let Some(app_handle) = app_handle {
        let project_id = ctx.project().id;
        app_handle.emit_stack_update(project_id, stack_id);
    }

    outcome
}

pub struct GetProjectStatus;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GetProjectStatusParameters {
    /// Optional filter for file changes.
    #[schemars(description = "
    <description>
        Optional filter for file changes.
        This can be used to limit the file changes returned in the project status.
    </description>

    <important_notes>
        The filter should be a list of file paths to include in the project status.
        If not provided, all file changes will be included.
    </important_notes>
    ")]
    pub filter_changes: Option<Vec<String>>,
}

impl Tool for GetProjectStatus {
    fn name(&self) -> String {
        "get_project_status".to_string()
    }

    fn description(&self) -> String {
        "
        <description>
            Get the current status of the project, including stacks and file changes.
        </description>
        "
        .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        let schema = schema_for!(GetProjectStatusParameters);
        serde_json::to_value(&schema).unwrap_or_default()
    }

    fn call(
        self: Arc<Self>,
        parameters: serde_json::Value,
        ctx: &mut CommandContext,
        _app_handle: Option<&tauri::AppHandle>,
    ) -> anyhow::Result<serde_json::Value> {
        let repo = ctx.gix_repo()?;
        let params: GetProjectStatusParameters = serde_json::from_value(parameters)
            .map_err(|e| anyhow::anyhow!("Failed to parse input parameters: {}", e))?;

        let paths = params
            .filter_changes
            .map(|f| f.into_iter().map(BString::from).collect::<Vec<BString>>());

        let value = get_project_status(ctx, &repo, paths).to_json("get_project_status");
        Ok(value)
    }
}

pub struct CreateBlankCommit;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateBlankCommitParameters {
    /// The commit message title.
    #[schemars(description = "
    <description>
        The commit message title.
        This is only a short summary of the commit.
    </description>

    <important_notes>
        The commit message title should be concise and descriptive.
        It is typically a single line that summarizes the changes made in the commit.
        For example: 'Fix issue with user login' or 'Update README with installation instructions'.
        Don't exceed 50 characters in length.
    </important_notes>
    ")]
    pub message_title: String,
    /// The commit message body.
    #[schemars(description = "
    <description>
        The commit message body.
        This is a more detailed description of the changes made in the commit.
    </description>

    <important_notes>
        The commit message body should provide context and details about the changes made.
        It should span multiple lines if necessary.
        A good description focuses on describing the 'what' of the changes.
        Don't make assumption about the 'why', only describe the changes in the context of the branch (and other commits if any).
    </important_notes>
    ")]
    pub message_body: String,
    /// The stack id to create the blank commit on.
    #[schemars(description = "
    <description>
        The stack id where the blank commit should be created.
    </description>

    <important_notes>
        The stack id should refer to an existing stack in the workspace.
    </important_notes>
    ")]
    pub stack_id: String,
    /// The ID of the commit to insert the blank commit on top of.
    #[schemars(description = "
    <description>
        The ID of the commit to insert the blank commit on top of.
    </description>

    <important_notes>
        This should be the ID of an existinf commit in the stack.
    </important_notes>
    ")]
    pub parent_id: String,
}

impl Tool for CreateBlankCommit {
    fn name(&self) -> String {
        "create_blank_commit".to_string()
    }

    fn description(&self) -> String {
        "
        <description>
            Create a blank commit on a specific stack in the workspace.
        </description>

        <important_notes>
            Use this tool when you want to split a commit into more parts.
            That you you can:
                1. Create one or more blank commits on top of an existing commit.
                2. Move the file changes from the existing commit to the new commit.
        </important_notes>
        "
        .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        let schema = schema_for!(CreateBlankCommitParameters);
        serde_json::to_value(&schema).unwrap_or_default()
    }

    fn call(
        self: Arc<Self>,
        parameters: serde_json::Value,
        ctx: &mut CommandContext,
        app_handle: Option<&tauri::AppHandle>,
    ) -> anyhow::Result<serde_json::Value> {
        let params: CreateBlankCommitParameters = serde_json::from_value(parameters)
            .map_err(|e| anyhow::anyhow!("Failed to parse input parameters: {}", e))?;

        match create_blank_commit(ctx, app_handle, params) {
            Ok(_) => Ok("Suceess".into()),
            Err(e) => Ok(error_to_json(&e, "create_blank_commit")),
        }
    }
}

pub fn create_blank_commit(
    ctx: &mut CommandContext,
    app_handle: Option<&tauri::AppHandle>,
    params: CreateBlankCommitParameters,
) -> Result<Vec<(gix::ObjectId, gix::ObjectId)>, anyhow::Error> {
    let stack_id = StackId::from_str(&params.stack_id)?;
    let commit_oid = gix::ObjectId::from_str(&params.parent_id)?;
    let commit_oid = commit_oid.to_git2();

    let message = format!(
        "{}\n\n{}",
        params.message_title.trim(),
        params.message_body.trim()
    );

    let commit_mapping = gitbutler_branch_actions::insert_blank_commit(
        ctx,
        stack_id,
        commit_oid,
        -1,
        Some(&message),
    )?;

    // If there's an app handle provided, emit an event to update the stack details in the UI.
    if let Some(app_handle) = app_handle {
        let project_id = ctx.project().id;
        app_handle.emit_stack_update(project_id, stack_id);
    }

    Ok(commit_mapping)
}

pub struct MoveFileChanges;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MoveFileChangesParameters {
    /// The commit id to move file changes from.
    #[schemars(description = "
    <description>
        The commit id of the commit to move file changes from.
    </description>

    <important_notes>
        The commit id should refer to a commit on the specified source stack.
    </important_notes>
    ")]
    pub source_commit_id: String,

    /// The stack id of the source commit.
    #[schemars(description = "
    <description>
        The stack id containing the source commit.
    </description>

    <important_notes>
        The stack id should refer to a stack in the workspace.
    </important_notes>
    ")]
    pub source_stack_id: String,

    /// The commit id to move file changes to.
    #[schemars(description = "
    <description>
        The commit id of the commit to move file changes to.
    </description>

    <important_notes>
        The commit id should refer to a commit on the specified destination stack.
    </important_notes>
    ")]
    pub destination_commit_id: String,

    /// The stack id of the destination commit.
    #[schemars(description = "
    <description>
        The stack id containing the destination commit.
    </description>

    <important_notes>
        The stack id should refer to a stack in the workspace.
    </important_notes>
    ")]
    pub destination_stack_id: String,

    /// The list of files to move.
    #[schemars(description = "
    <description>
        The list of file paths to move from the source commit to the destination commit.
    </description>

    <important_notes>
        The file paths should be relative to the workspace root.
        The file paths should be contained in the source commit.
        Only the specified files will be moved.
    </important_notes>
    ")]
    pub files: Vec<String>,
}

impl Tool for MoveFileChanges {
    fn name(&self) -> String {
        "move_file_changes".to_string()
    }

    fn description(&self) -> String {
        "
        <description>
            Move file changes from one commit to another in the workspace.
        </description>

        <important_notes>
            Use this tool when you want to move file changes from one commit to another.
            This is useful when you want to split a commit into more parts.
        </important_notes>
        "
        .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        let schema = schema_for!(MoveFileChangesParameters);
        serde_json::to_value(&schema).unwrap_or_default()
    }

    fn call(
        self: Arc<Self>,
        parameters: serde_json::Value,
        ctx: &mut CommandContext,
        app_handle: Option<&tauri::AppHandle>,
    ) -> anyhow::Result<serde_json::Value> {
        let params: MoveFileChangesParameters = serde_json::from_value(parameters)
            .map_err(|e| anyhow::anyhow!("Failed to parse input parameters: {}", e))?;

        match move_file_changes(ctx, app_handle, params) {
            Ok(_) => Ok("Success".into()),
            Err(e) => Ok(error_to_json(&e, "move_file_changes")),
        }
    }
}

pub fn move_file_changes(
    ctx: &mut CommandContext,
    app_handle: Option<&tauri::AppHandle>,
    params: MoveFileChangesParameters,
) -> Result<Vec<(gix::ObjectId, gix::ObjectId)>, anyhow::Error> {
    let source_commit_id = gix::ObjectId::from_str(&params.source_commit_id)?;
    let source_stack_id = StackId::from_str(&params.source_stack_id)?;
    let destination_commit_id = gix::ObjectId::from_str(&params.destination_commit_id)?;
    let destination_stack_id = StackId::from_str(&params.destination_stack_id)?;

    let changes = params
        .files
        .iter()
        .map(|f| but_workspace::DiffSpec {
            path: BString::from(f.as_str()),
            previous_path: None,
            hunk_headers: vec![],
        })
        .collect::<Vec<_>>();

    let result = but_workspace::move_changes_between_commits(
        ctx,
        source_stack_id,
        source_commit_id,
        destination_stack_id,
        destination_commit_id,
        changes,
        ctx.app_settings().context_lines,
    )?;

    let vb_state = VirtualBranchesHandle::new(ctx.project().gb_dir());
    gitbutler_branch_actions::update_workspace_commit(&vb_state, ctx)?;

    // If there's an app handle provided, emit an event to update the stack details in the UI.
    if let Some(app_handle) = app_handle {
        let project_id = ctx.project().id;
        app_handle.emit_stack_update(project_id, source_stack_id);
        app_handle.emit_stack_update(project_id, destination_stack_id);
    }

    Ok(result.replaced_commits)
}

pub struct GetCommitDetails;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GetCommitDetailsParameters {
    /// The commit id to get details for.
    #[schemars(description = "
    <description>
        The commit id of the commit to get details for.
    </description>

    <important_notes>
        The commit id should refer to a commit in the workspace.
    </important_notes>
    ")]
    pub commit_id: String,
}

impl Tool for GetCommitDetails {
    fn name(&self) -> String {
        "get_commit_details".to_string()
    }

    fn description(&self) -> String {
        "
        <description>
            Get details of a specific commit in the workspace.
        </description>

        <important_notes>
            This tool allows you to retrieve detailed information about a specific commit in the workspace.
            Use this tool to get the information about the files changed in the commit.
            You'll want to use this tool before moving file changes from one commit to another.
        </important_notes>
        "
        .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        let schema = schema_for!(GetCommitDetailsParameters);
        serde_json::to_value(&schema).unwrap_or_default()
    }

    fn call(
        self: Arc<Self>,
        parameters: serde_json::Value,
        ctx: &mut CommandContext,
        _app_handle: Option<&tauri::AppHandle>,
    ) -> anyhow::Result<serde_json::Value> {
        let params: GetCommitDetailsParameters = serde_json::from_value(parameters)
            .map_err(|e| anyhow::anyhow!("Failed to parse input parameters: {}", e))?;

        let file_changes = commit_details(ctx, params).to_json("commit_details");

        Ok(file_changes)
    }
}

pub fn commit_details(
    ctx: &mut CommandContext,
    params: GetCommitDetailsParameters,
) -> anyhow::Result<Vec<FileChange>> {
    let repo = ctx.gix_repo()?;
    let commit_id = gix::ObjectId::from_str(&params.commit_id)?;

    let changes = but_core::diff::ui::commit_changes_by_worktree_dir(&repo, commit_id)?;
    let changes: Vec<but_core::TreeChange> = changes
        .changes
        .into_iter()
        .map(|change| change.into())
        .collect();

    let diff = unified_diff_for_changes(&repo, changes, ctx.app_settings().context_lines)?;
    let file_changes = get_file_changes(&diff, vec![]);

    Ok(file_changes)
}

fn ref_metadata_toml(project: &Project) -> anyhow::Result<VirtualBranchesTomlMetadata> {
    VirtualBranchesTomlMetadata::from_path(project.gb_dir().join("virtual_branches.toml"))
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RichHunk {
    /// The diff string.
    pub diff: String,
    /// The stack ID this hunk is assigned to, if any.
    pub assigned_to_stack: Option<but_workspace::StackId>,
    /// The locks this hunk has, if any.
    pub dependency_locks: Vec<but_hunk_dependency::ui::HunkLock>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SimpleCommit {
    /// The commit sha.
    #[serde(with = "gitbutler_serde::object_id")]
    pub id: gix::ObjectId,
    /// The commit message.
    pub message_title: String,
    /// The commit message body.
    pub message_body: String,
}

impl From<but_workspace::ui::Commit> for SimpleCommit {
    fn from(commit: but_workspace::ui::Commit) -> Self {
        let message_str = commit.message.to_string();
        let mut lines = message_str.lines();
        let message_title = lines.next().unwrap_or_default().to_string();
        let mut message_body = lines.collect::<Vec<_>>().join("\n");
        // Remove leading empty lines from the body
        while message_body.starts_with('\n') || message_body.starts_with("\r\n") {
            message_body = message_body
                .trim_start_matches('\n')
                .trim_start_matches("\r\n")
                .to_string();
        }
        SimpleCommit {
            id: commit.id,
            message_title,
            message_body,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SimpleBranch {
    /// The name of the branch.
    pub name: String,
    /// The description of the branch.
    pub description: Option<String>,
    /// The commits in the branch.
    pub commits: Vec<SimpleCommit>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SimpleStack {
    /// The stack ID.
    pub id: but_workspace::StackId,
    /// The name of the stack.
    pub name: String,
    /// The branches in the stack.
    pub branches: Vec<SimpleBranch>,
}
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChange {
    /// The path of the file that has changed.
    pub path: String,
    /// The file change status
    pub status: String,
    /// The hunk changes in the file.
    pub hunks: Vec<RichHunk>,
}

impl ToolResult for Result<Vec<FileChange>, anyhow::Error> {
    fn to_json(&self, action_identifier: &str) -> serde_json::Value {
        result_to_json(self, action_identifier, "Vec<FileChange>")
    }
}

/// Represents the status of a project, including applied stacks and file changes.
///
/// The shape of this struct is designed to be serializable and as simple as possible for use in LLM context.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectStatus {
    /// List of stacks applied to the project's workspace
    pub stacks: Vec<SimpleStack>,
    /// Unified diff changes that could be committed.
    pub file_changes: Vec<FileChange>,
}

impl ToolResult for Result<ProjectStatus, anyhow::Error> {
    fn to_json(&self, action_identifier: &str) -> serde_json::Value {
        result_to_json(self, action_identifier, "ProjectStatus")
    }
}

pub fn get_project_status(
    ctx: &mut CommandContext,
    repo: &gix::Repository,
    filter_changes: Option<Vec<BString>>,
) -> anyhow::Result<ProjectStatus> {
    let stacks = stacks(ctx, repo)?;
    let stacks = entries_to_simple_stacks(&stacks, ctx, repo)?;

    let file_changes = get_filtered_changes(ctx, repo, filter_changes)?;

    Ok(ProjectStatus {
        stacks,
        file_changes,
    })
}

pub fn get_filtered_changes(
    ctx: &mut CommandContext,
    repo: &gix::Repository,
    filter_changes: Option<Vec<BString>>,
) -> Result<Vec<FileChange>, anyhow::Error> {
    let worktree = but_core::diff::worktree_changes(repo)?;
    let changes = if let Some(filter) = filter_changes {
        worktree
            .changes
            .into_iter()
            .filter(|change| filter.iter().any(|f| *f == change.path))
            .collect::<Vec<_>>()
    } else {
        worktree.changes.clone()
    };
    let diff = unified_diff_for_changes(repo, changes, ctx.app_settings().context_lines)?;
    let (assignments, _) = but_hunk_assignment::assignments_with_fallback(
        ctx,
        true,
        None::<Vec<but_core::TreeChange>>,
        None,
    )
    .map_err(|err| serde_error::Error::new(&*err))?;
    let file_changes = get_file_changes(&diff, assignments.clone());
    Ok(file_changes)
}

fn entries_to_simple_stacks(
    entries: &[StackEntry],
    ctx: &mut CommandContext,
    repo: &gix::Repository,
) -> anyhow::Result<Vec<SimpleStack>> {
    let mut stacks = vec![];
    let vb_state = VirtualBranchesHandle::new(ctx.project().gb_dir());
    for entry in entries {
        let stack = vb_state.get_stack(entry.id)?;
        let branches = stack.branches();
        let branches = branches.iter().filter(|b| !b.archived);
        let mut simple_branches = vec![];
        for branch in branches {
            let commits = but_workspace::local_and_remote_commits(ctx, repo, branch, &stack)?;

            if commits.is_empty() {
                continue;
            }

            let simple_commits = commits
                .into_iter()
                .map(SimpleCommit::from)
                .collect::<Vec<_>>();

            simple_branches.push(SimpleBranch {
                name: branch.name.to_string(),
                description: branch.description.clone(),
                commits: simple_commits,
            });
        }
        if simple_branches.is_empty() {
            continue;
        }

        stacks.push(SimpleStack {
            id: entry.id,
            name: entry.name().unwrap_or_default().to_string(),
            branches: simple_branches,
        });
    }
    Ok(stacks)
}

fn get_file_changes(
    changes: &[(TreeChange, UnifiedDiff)],
    assingments: Vec<but_hunk_assignment::HunkAssignment>,
) -> Vec<FileChange> {
    let mut file_changes = vec![];
    for (change, unified_diff) in changes.iter() {
        match unified_diff {
            but_core::UnifiedDiff::Patch { hunks, .. } => {
                let path = change.path.to_string();
                let status = match &change.status {
                    but_core::TreeStatus::Addition { .. } => "added".to_string(),
                    but_core::TreeStatus::Deletion { .. } => "deleted".to_string(),
                    but_core::TreeStatus::Modification { .. } => "modified".to_string(),
                    but_core::TreeStatus::Rename { previous_path, .. } => {
                        format!("renamed from {}", previous_path)
                    }
                };

                let hunks = hunks
                    .iter()
                    .map(|hunk| {
                        let diff = hunk.diff.to_string();
                        let assignment = assingments
                            .iter()
                            .find(|a| {
                                a.path_bytes == change.path && a.hunk_header == Some(hunk.into())
                            })
                            .map(|a| (a.stack_id, a.hunk_locks.clone()));

                        let (assigned_to_stack, dependency_locks) =
                            if let Some((stack_id, locks)) = assignment {
                                let locks = locks.unwrap_or_default();
                                (stack_id, locks)
                            } else {
                                (None, vec![])
                            };

                        RichHunk {
                            diff,
                            assigned_to_stack,
                            dependency_locks,
                        }
                    })
                    .collect::<Vec<_>>();

                file_changes.push(FileChange {
                    path,
                    status,
                    hunks,
                });
            }
            _ => continue,
        }
    }

    file_changes
}

fn unified_diff_for_changes(
    repo: &gix::Repository,
    changes: Vec<but_core::TreeChange>,
    context_lines: u32,
) -> anyhow::Result<Vec<(but_core::TreeChange, but_core::UnifiedDiff)>> {
    changes
        .into_iter()
        .map(|tree_change| {
            tree_change
                .unified_diff(repo, context_lines)
                .map(|diff| (tree_change, diff.expect("no submodule")))
        })
        .collect::<Result<Vec<_>, _>>()
}

#[derive(Debug, serde::Serialize, serde::Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AbsorbSpec {
    /// The title of the commti to use in the amended commit.
    #[schemars(description = "
    <description>
        The title of the commit to use in the amended commit.
    </description>
    
    <important_notes>
        The title should be concise and descriptive.
        Don't use more than 50 characters.
        It should be differente from the original commit title only if needed.
    </important_notes>
    ")]
    pub commit_title: String,
    /// The description of the commit to use in the amended commit.
    #[schemars(description = "
    <description>
        The description of the commit to use in the amended commit.
    </description>

    <important_notes>
        The description should provide context and details about the changes made.
        It should span multiple lines if necessary.
        A good description focuses on describing the 'what' of the changes.
        Don't make assumption about the 'why', only describe the changes in the context of the branch (and other commits if any).
    </important_notes>
    ")]
    pub commit_description: String,
}
