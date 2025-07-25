import { goto } from '$app/navigation';
import { invoke } from '$lib/backend/ipc';
import { showError } from '$lib/notifications/toasts';
import { Project, type CloudProject } from '$lib/project/project';
import { sleep } from '$lib/utils/sleep';
import { persisted } from '@gitbutler/shared/persisted';
import * as toasts from '@gitbutler/ui/toasts';
import { open } from '@tauri-apps/plugin-dialog';
import { plainToInstance } from 'class-transformer';
import { derived, get, writable, type Readable } from 'svelte/store';
import type { HttpClient } from '@gitbutler/shared/network/httpClient';

export type ProjectInfo = {
	is_exclusive: boolean;
	db_error?: string;
	headsup?: string;
};

export class ProjectsService {
	private persistedId = persisted<string | undefined>(undefined, 'lastProject');
	readonly projects = writable<Project[] | undefined>(undefined, (set) => {
		sleep(100).then(() => {
			this.loadAll()
				.then((projects) => {
					this.error.set(undefined);
					set(projects);
				})
				.catch((err) => {
					this.error.set(err);
					showError('Failed to load projects', err);
				});
		});
	});
	readonly error = writable();

	constructor(
		private homeDir: string | undefined,
		private httpClient: HttpClient
	) {}

	private async loadAll() {
		return await invoke<Project[]>('list_projects').then((p) => plainToInstance(Project, p));
	}

	async reload(): Promise<void> {
		this.projects.set(await this.loadAll());
	}

	async setActiveProject(projectId: string): Promise<ProjectInfo> {
		const info = await invoke<ProjectInfo>('set_project_active', { id: projectId });
		await this.reload();
		return info;
	}

	async getProject(projectId: string, noValidation?: boolean) {
		return plainToInstance(Project, await invoke('get_project', { id: projectId, noValidation }));
	}

	#projectStores = new Map<string, Readable<Project | undefined>>();
	getProjectStore(projectId: string) {
		let store = this.#projectStores.get(projectId);
		if (store) return store;

		store = derived(this.projects, (projects) => {
			return projects?.find((p) => p.id === projectId);
		});
		this.#projectStores.set(projectId, store);
		return store;
	}

	async updateProject(project: Project & { unset_bool?: boolean; unset_forge_override?: boolean }) {
		await invoke('update_project', { project: project });
		await this.reload();
	}

	async add(path: string) {
		const project = plainToInstance(Project, await invoke('add_project', { path }));
		await this.reload();
		return project;
	}

	async deleteProject(id: string) {
		await invoke('delete_project', { id });
		await this.reload();
	}

	async promptForDirectory(): Promise<string | undefined> {
		const selectedPath = open({ directory: true, recursive: true, defaultPath: this.homeDir });
		if (selectedPath) {
			if (selectedPath === null) return;
			if (Array.isArray(selectedPath) && selectedPath.length !== 1) return;
			return Array.isArray(selectedPath) ? selectedPath[0] : ((await selectedPath) ?? undefined);
		}
	}

	async openProjectInNewWindow(projectId: string) {
		await invoke('open_project_in_window', { id: projectId });
	}

	async relocateProject(projectId: string): Promise<void> {
		const path = await this.getValidPath();
		if (!path) return;

		try {
			const project = await this.getProject(projectId, true);
			project.path = path;
			await this.updateProject(project);
			toasts.success(`Project ${project.title} relocated`);

			goto(`/${project.id}/board`);
		} catch (error: any) {
			showError('Failed to relocate project:', error.message);
		}
	}

	async addProject(path?: string) {
		if (!path) {
			path = await this.getValidPath();
			if (!path) return;
		}

		try {
			const project = await this.add(path);
			if (!project) return;
			toasts.success(`Project ${project.title} created`);
			// linkProjectModal?.show(project.id);
			goto(`/${project.id}/board`);
		} catch (e: any) {
			showError('There was an error while adding project', e.message);
		}
	}

	async getValidPath(): Promise<string | undefined> {
		const path = await this.promptForDirectory();
		if (!path) return undefined;
		if (!this.validateProjectPath(path)) return undefined;
		return path;
	}

	validateProjectPath(path: string) {
		if (/^\\\\wsl.localhost/i.test(path)) {
			const errorMsg =
				'For WSL2 projects, install the Linux version of GitButler inside of your WSL2 distro';
			console.error(errorMsg);
			showError('Use the Linux version of GitButler', errorMsg);

			return false;
		}

		if (/^\\\\/i.test(path)) {
			const errorMsg =
				'Using git across a network is not recommended. Either clone the repo locally, or use the NET USE command to map a network drive';
			console.error(errorMsg);
			showError('UNC Paths are not directly supported', errorMsg);

			return false;
		}

		return true;
	}

	getLastOpenedProject() {
		return get(this.persistedId);
	}

	setLastOpenedProject(projectId: string) {
		this.persistedId.set(projectId);
	}

	unsetLastOpenedProject() {
		this.persistedId.set(undefined);
	}

	async createCloudProject(params: {
		name: string;
		description?: string;
		uid?: string;
	}): Promise<CloudProject> {
		return await this.httpClient.post('projects.json', {
			body: params
		});
	}

	async updateCloudProject(
		repositoryId: string,
		params: {
			name: string;
			description?: string;
		}
	): Promise<CloudProject> {
		return await this.httpClient.put(`projects/${repositoryId}.json`, {
			body: params
		});
	}

	async getCloudProject(repositoryId: string): Promise<CloudProject> {
		return await this.httpClient.get(`projects/${repositoryId}.json`);
	}

	async getActiveProject(): Promise<Project | undefined> {
		return await invoke('get_active_project');
	}
}
