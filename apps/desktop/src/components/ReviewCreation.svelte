<script lang="ts" module>
	export interface CreatePrParams {
		stackId: string;
		branchName: string;
		title: string;
		body: string;
		draft: boolean;
		upstreamBranchName: string | undefined;
	}
</script>

<script lang="ts">
	import PrTemplateSection from '$components/PrTemplateSection.svelte';
	import AsyncRender from '$components/v3/AsyncRender.svelte';
	import MessageEditor from '$components/v3/editor/MessageEditor.svelte';
	import MessageEditorInput from '$components/v3/editor/MessageEditorInput.svelte';
	import { AIService } from '$lib/ai/service';
	import { PostHogWrapper } from '$lib/analytics/posthog';
	import { BaseBranch } from '$lib/baseBranch/baseBranch';
	import { type Commit } from '$lib/branches/v3';
	import { projectAiGenEnabled } from '$lib/config/config';
	import { ButRequestDetailsService } from '$lib/forge/butRequestDetailsService';
	import { DefaultForgeFactory } from '$lib/forge/forgeFactory.svelte';
	import { mapErrorToToast } from '$lib/forge/github/errorMap';
	import { GitHubPrService } from '$lib/forge/github/githubPrService.svelte';
	import { type PullRequest } from '$lib/forge/interface/types';
	import { PrPersistedStore, PrTemplateStore } from '$lib/forge/prContents';
	import {
		BrToPrService,
		updatePrDescriptionTables as updatePrStackInfo
	} from '$lib/forge/shared/prFooter';
	import { TemplateService } from '$lib/forge/templateService';
	import { StackPublishingService } from '$lib/history/stackPublishingService';
	import { showError, showToast } from '$lib/notifications/toasts';
	import { ProjectsService } from '$lib/project/projectsService';
	import { RemotesService } from '$lib/remotes/remotesService';
	import { requiresPush } from '$lib/stacks/stack';
	import { StackService } from '$lib/stacks/stackService.svelte';
	import { UiState } from '$lib/state/uiState.svelte';
	import { TestId } from '$lib/testing/testIds';
	import { parseRemoteUrl } from '$lib/url/gitUrl';
	import { UserService } from '$lib/user/userService';
	import { getBranchNameFromRef } from '$lib/utils/branch';
	import { splitMessage } from '$lib/utils/commitMessage';
	import { sleep } from '$lib/utils/sleep';
	import { getContext } from '@gitbutler/shared/context';
	import { persisted } from '@gitbutler/shared/persisted';
	import { error } from '@gitbutler/ui/toasts';
	import { isDefined } from '@gitbutler/ui/utils/typeguards';
	import { tick } from 'svelte';

	type Props = {
		projectId: string;
		stackId: string;
		branchName: string;
		prNumber?: number;
		reviewId?: string;
		onClose: () => void;
	};

	const { projectId, stackId, branchName, prNumber, reviewId, onClose }: Props = $props();

	const baseBranch = getContext(BaseBranch);
	const forge = getContext(DefaultForgeFactory);
	const prService = $derived(forge.current.prService);
	const stackPublishingService = getContext(StackPublishingService);
	const butRequestDetailsService = getContext(ButRequestDetailsService);
	const brToPrService = getContext(BrToPrService);
	const posthog = getContext(PostHogWrapper);
	const stackService = getContext(StackService);
	const projectsService = getContext(ProjectsService);
	const userService = getContext(UserService);
	const aiService = getContext(AIService);
	const remotesService = getContext(RemotesService);
	const uiState = getContext(UiState);
	const templateService = getContext(TemplateService);

	const user = userService.user;
	const project = projectsService.getProjectStore(projectId);

	const [publishBranch, branchPublishing] = stackService.publishBranch;
	const [pushStack, stackPush] = stackService.pushStack;

	const branchesResult = $derived(stackService.branches(projectId, stackId));
	const branches = $derived(branchesResult.current.data || []);
	const branchParentResult = $derived(
		stackService.branchParentByName(projectId, stackId, branchName)
	);
	const branchParent = $derived(branchParentResult.current.data);
	const branchParentDetailsResult = $derived(
		branchParent ? stackService.branchDetails(projectId, stackId, branchParent.name) : undefined
	);
	const branchParentDetails = $derived(branchParentDetailsResult?.current.data);
	const branchDetailsResult = $derived(stackService.branchDetails(projectId, stackId, branchName));
	const branchDetails = $derived(branchDetailsResult.current.data);
	const commitsResult = $derived(stackService.commits(projectId, stackId, branchName));
	const commits = $derived(commitsResult.current.data || []);

	const canPublish = stackPublishingService.canPublish;

	const prResult = $derived(prNumber ? prService?.get(prNumber) : undefined);
	const pr = $derived(prResult?.current.data);

	const forgeBranch = $derived(branchName ? forge.current.branch(branchName) : undefined);
	const baseBranchName = $derived(baseBranch.shortName);

	const createDraft = persisted<boolean>(false, 'createDraftPr');
	const createButlerRequest = persisted<boolean>(false, 'createButlerRequest');
	const createPullRequest = persisted<boolean>(true, 'createPullRequest');

	const pushBeforeCreate = $derived(
		!forgeBranch || (branchDetails ? requiresPush(branchDetails.pushStatus) : true)
	);

	let titleInput = $state<HTMLTextAreaElement | undefined>(undefined);
	let messageEditor = $state<MessageEditor>();

	// AI things
	const aiGenEnabled = projectAiGenEnabled(projectId);
	let aiConfigurationValid = $state(false);
	const canUseAI = $derived($aiGenEnabled && aiConfigurationValid);
	let aiIsLoading = $state(false);

	$effect(() => {
		aiService.validateConfiguration().then((valid) => {
			aiConfigurationValid = valid;
		});
	});

	let isCreatingReview = $state<boolean>(false);
	const isExecuting = $derived(
		branchPublishing.current.isLoading ||
			stackPush.current.isLoading ||
			aiIsLoading ||
			isCreatingReview
	);

	const canPublishBR = $derived(!!($canPublish && branchName && !reviewId));
	const canPublishPR = $derived(!!(forge.current.authenticated && !pr));

	async function getDefaultTitle(commits: Commit[]): Promise<string> {
		if (commits.length === 1) {
			const commitMessage = commits[0]!.message;
			const { title } = splitMessage(commitMessage);
			return title;
		}
		return branchName;
	}

	const templateStore = $derived(
		new PrTemplateStore(projectId, forge.current.name, templateService)
	);
	const templateEnabled = $derived(templateStore.templateEnabled);
	const templatePath = $derived(templateStore.templatePath);

	async function getDefaultBody(commits: Commit[]): Promise<string> {
		if ($templateEnabled && $templatePath) {
			return await templateStore.getTemplateContent($templatePath);
		}
		if (commits.length === 1) {
			return splitMessage(commits[0]!.message).description;
		}
		return '';
	}

	const prTitle = $derived(
		new PrPersistedStore({
			cacheKey: 'prtitle_' + projectId + '_' + branchName,
			commits,
			defaultFn: getDefaultTitle
		})
	);

	const prBody = $derived(
		new PrPersistedStore({
			cacheKey: 'prbody' + projectId + '_' + branchName,
			commits,
			defaultFn: getDefaultBody
		})
	);

	$effect(() => {
		prBody.setDefault(commits);
		prTitle.setDefault(commits);
	});

	async function pushIfNeeded(): Promise<string | undefined> {
		let upstreamBranchName: string | undefined = branchName;
		if (pushBeforeCreate) {
			const firstPush = branchDetails?.pushStatus === 'completelyUnpushed';
			const pushResult = await pushStack({
				projectId,
				stackId,
				withForce: branchDetails?.pushStatus === 'unpushedCommitsRequiringForce'
			});

			if (pushResult) {
				upstreamBranchName = getBranchNameFromRef(pushResult.refname, pushResult.remote);
			}

			if (firstPush) {
				// TODO: fix this hack for reactively available prService.
				await sleep(500);
			}
		}

		return upstreamBranchName;
	}

	function shouldAddPrBody() {
		// If there is a branch review already, then the BR to PR sync will
		// update the PR description for us.
		if (reviewId) return false;
		// If we can't publish a BR, then we must add the PR description
		if (!canPublishBR) return true;
		// If the user wants to create a butler request then we don't want
		// to add the PR body as it will be handled by the syncing
		return !$createButlerRequest;
	}

	export async function createReview() {
		if (isExecuting) return;
		if (!$user) return;

		const effectivePRBody = (await messageEditor?.getPlaintext()) ?? '';
		// Declare early to have them inside the function closure, in case
		// the component unmounts or updates.
		const closureStackId = stackId;
		const closureBranchName = branchName;
		const title = $prTitle;
		const body = shouldAddPrBody() ? effectivePRBody : '';
		const draft = $createDraft;

		try {
			isCreatingReview = true;
			await tick();

			const upstreamBranchName = await pushIfNeeded();

			let newReviewId: string | undefined;
			let newPrNumber: number | undefined;

			// Even if createButlerRequest is false, if we _cant_ create a PR, then
			// We want to always create the BR, and vice versa.
			if ((canPublishBR && $createButlerRequest) || !canPublishPR) {
				const reviewId = await publishBranch({
					projectId,
					stackId,
					topBranch: branchName,
					user: $user
				});
				if (!reviewId) {
					posthog.capture('Butler Review Creation Failed');
					return;
				}
				posthog.capture('Butler Review Created');
				butRequestDetailsService.setDetails(reviewId, $prTitle, effectivePRBody);
			}

			if ((canPublishPR && $createPullRequest) || !canPublishBR) {
				const pr = await createPr({
					stackId: closureStackId,
					branchName: closureBranchName,
					title,
					body,
					draft,
					upstreamBranchName
				});
				newPrNumber = pr?.number;
			}

			if (newReviewId && newPrNumber && $project?.api?.repository_id) {
				brToPrService.refreshButRequestPrDescription(
					newPrNumber,
					newReviewId,
					$project.api.repository_id
				);
			}

			prBody.reset();
			prTitle.reset();
			uiState.project(projectId).exclusiveAction.set(undefined);
		} finally {
			isCreatingReview = false;
		}
		onClose();
	}

	async function createPr(params: CreatePrParams): Promise<PullRequest | undefined> {
		if (!forge) {
			error('Pull request service not available');
			return;
		}

		// All ids that existed prior to creating a new one (including archived).
		const prNumbers = branches.map((branch) => branch.prNumber);

		try {
			if (!baseBranchName) {
				error('No base branch name determined');
				return;
			}

			if (!params.upstreamBranchName) {
				error('No upstream branch name determined');
				return;
			}

			if (!prService) {
				error('Pull request service not available');
				return;
			}

			// Find the index of the current branch so we know where we want to point the pr.
			const currentIndex = branches.findIndex((b) => b.name === params.branchName);
			if (currentIndex === -1) {
				throw new Error('Branch index not found.');
			}

			// Use base branch as base unless it's part of stack and should be be pointing
			// to the preceding branch. Ensuring we're not using `archived` branches as base.
			let base = baseBranch?.shortName || 'master';

			if (
				branchParent &&
				branchParent.prNumber &&
				branchParentDetails &&
				branchParentDetails.pushStatus !== 'integrated'
			) {
				base = branchParent.name;
			}

			const pushRemoteName = baseBranch.actualPushRemoteName();
			const allRemotes = await remotesService.remotes(projectId);
			const pushRemote = allRemotes.find((r) => r.name === pushRemoteName);
			const pushRemoteUrl = pushRemote?.url;

			const repoInfo = parseRemoteUrl(pushRemoteUrl);

			const upstreamName =
				prService instanceof GitHubPrService
					? repoInfo?.owner
						? `${repoInfo.owner}:${params.upstreamBranchName}`
						: params.upstreamBranchName
					: params.upstreamBranchName;

			const pr = await prService.createPr({
				title: params.title,
				body: params.body,
				draft: params.draft,
				baseBranchName: base,
				upstreamName
			});

			// Store the new pull request number with the branch data.
			await stackService.updateBranchPrNumber({
				projectId,
				stackId: params.stackId,
				branchName: params.branchName,
				prNumber: pr.number
			});

			// If we now have two or more pull requests we add a stack table to the description.
			prNumbers[currentIndex] = pr.number;
			const definedPrNumbers = prNumbers.filter(isDefined);
			if (definedPrNumbers.length > 0) {
				updatePrStackInfo(prService, definedPrNumbers);
			}
		} catch (err: any) {
			console.error(err);
			const toast = mapErrorToToast(err);
			if (toast) showToast(toast);
			else showError('Error while creating pull request', err);
		}
	}

	const isCreateButtonEnabled = $derived.by(() => {
		if ((canPublishBR && $createButlerRequest) || !canPublishPR) {
			return true;
		}
		if ((canPublishPR && $createPullRequest) || !canPublishBR) {
			return true;
		}
		return false;
	});

	async function onAiButtonClick() {
		if (!aiGenEnabled || aiIsLoading) return;

		aiIsLoading = true;
		await tick();

		let firstToken = true;

		try {
			const description = await aiService?.describePR({
				title: $prTitle,
				body: $prBody,
				commitMessages: commits.map((c) => c.message),
				prBodyTemplate: prBody.default,
				onToken: (token) => {
					if (firstToken) {
						prBody.reset();
						firstToken = false;
					}
					prBody.append(token);
					messageEditor?.setText($prBody);
				}
			});

			if (description) {
				prBody.set(description);
				messageEditor?.setText($prBody);
			}
		} finally {
			aiIsLoading = false;
			await tick();
		}
	}

	export const imports = {
		get creationEnabled() {
			return isCreateButtonEnabled;
		},
		get isLoading() {
			return isExecuting;
		}
	};
</script>

<div class="pr-editor">
	<AsyncRender>
		<PrTemplateSection
			{projectId}
			{templateStore}
			disabled={isExecuting}
			onselect={(value) => {
				prBody.set(value);
				messageEditor?.setText(value);
			}}
		/>
		<div class="pr-fields">
			<MessageEditorInput
				testId={TestId.ReviewTitleInput}
				bind:ref={titleInput}
				value={$prTitle}
				onchange={(value) => {
					prTitle.set(value);
				}}
				onkeydown={(e: KeyboardEvent) => {
					if (e.key === 'Enter' || (e.key === 'Tab' && !e.shiftKey)) {
						e.preventDefault();
						messageEditor?.focus();
					}

					if (e.key === 'Escape') {
						e.preventDefault();
						onClose();
					}
				}}
				placeholder="PR title"
				showCount={false}
				oninput={(e: Event) => {
					const target = e.target as HTMLInputElement;
					prTitle.set(target.value);
				}}
			/>
			<MessageEditor
				isPrCreation
				bind:this={messageEditor}
				testId={TestId.ReviewDescriptionInput}
				{projectId}
				disabled={isExecuting}
				initialValue={$prBody}
				enableFileUpload
				enableSmiles
				placeholder="PR Description"
				{onAiButtonClick}
				{canUseAI}
				{aiIsLoading}
				onChange={(text: string) => {
					prBody.set(text);
				}}
				onKeyDown={(e: KeyboardEvent) => {
					if (e.key === 'Tab' && e.shiftKey) {
						e.preventDefault();
						titleInput?.focus();
						return true;
					}

					if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
						e.preventDefault();
						createReview();
						return true;
					}

					if (e.key === 'Escape') {
						e.preventDefault();
						onClose();
						return true;
					}

					return false;
				}}
			/>
		</div>
	</AsyncRender>
</div>

<style lang="postcss">
	.pr-editor {
		display: flex;
		flex: 1;
		flex-direction: column;
		overflow: hidden;
		gap: 10px;
	}

	.pr-fields {
		display: flex;
		flex-direction: column;
		height: 100%;
		overflow: hidden;
	}
</style>
