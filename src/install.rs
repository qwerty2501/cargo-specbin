use nanoid::nanoid;
use tokio::fs;

use crate::fronts::MultiProgress;
use crate::paths::isobin_config::IsobinConfigPathError;
use crate::paths::workspace::Workspace;
use crate::paths::workspace::WorkspaceProvider;
use crate::providers::cargo::CargoConfig;
use crate::providers::cargo::CargoInstallTarget;
use crate::utils::fs_ext;
use crate::{paths::project::Project, providers::cargo::CargoInstallerFactory};
use std::collections::HashSet;
use std::path::PathBuf;

use super::*;
use std::sync::Arc;

#[derive(PartialEq)]
pub enum InstallMode {
    All,
    SpecificInstallTargetsOnly {
        specific_install_targets: Vec<String>,
    },
}

#[derive(new)]
pub struct InstallService {
    #[allow(dead_code)]
    project: Project,
    workspace_provider: WorkspaceProvider,
}

impl Default for InstallService {
    fn default() -> Self {
        let project = Project::default();
        Self {
            workspace_provider: WorkspaceProvider::new(project.clone()),
            project,
        }
    }
}

impl InstallService {
    #[allow(unused_variables)]
    pub async fn install(
        &self,
        service_option: &ServiceOption,
        install_service_option: &InstallServiceOption,
    ) -> Result<()> {
        let isobin_config = service_option.isobin_config();
        let isobin_config_dir = service_option
            .isobin_config_path()
            .parent()
            .ok_or(IsobinConfigPathError::NotFoundIsobinConfig)?;
        let isobin_config_dir = fs::canonicalize(isobin_config_dir).await?;
        let workspace = self
            .workspace_provider
            .base_unique_workspace_dir_from_isobin_config_dir(&isobin_config_dir)
            .await?;
        let tmp_workspace = Workspace::new(
            workspace.id().clone(),
            workspace.cache_dir().join(nanoid!()),
            workspace.cache_dir().clone(),
        );
        fs_ext::create_dir_if_not_exists(tmp_workspace.base_dir()).await?;
        let cargo_installer_factory = CargoInstallerFactory::new(tmp_workspace.clone());
        let install_runner_provider = InstallRunnerProvider::default();
        let cargo_runner = install_runner_provider
            .make_cargo_runner(&cargo_installer_factory, isobin_config.cargo())
            .await?;
        self.run_each_installs(&workspace, &tmp_workspace, vec![cargo_runner])
            .await
    }

    async fn run_each_installs(
        &self,
        workspace: &Workspace,
        tmp_workspace: &Workspace,
        runners: Vec<Arc<dyn InstallRunner>>,
    ) -> Result<()> {
        join_futures!(runners.iter().map(|r| r.run_installs()))
            .await
            .map_err(InstallServiceError::MultiInstall)?;
        let mut keys = HashSet::new();
        let mut duplicates = vec![];
        for file_name in join_futures!(runners.iter().map(|r| r.bin_paths()))
            .await
            .map_err(InstallServiceError::MultiInstall)?
            .into_iter()
            .flatten()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
        {
            if !keys.insert(file_name.clone()) {
                duplicates.push(file_name);
            }
        }
        if !duplicates.is_empty() {
            Err(InstallServiceError::new_duplicate_bin(duplicates).into())
        } else {
            join_futures!(runners.iter().map(|r| r.install_bin_path()))
                .await
                .map_err(InstallServiceError::MultiInstall)?;
            let tmp_dir = workspace.cache_dir().join(nanoid!());
            let need_tmp = workspace.base_dir().exists();
            if need_tmp {
                fs::rename(workspace.base_dir(), &tmp_dir).await?;
            }
            match fs::rename(tmp_workspace.base_dir(), workspace.base_dir()).await {
                Ok(_) => {}
                Err(err) => {
                    if need_tmp {
                        fs::rename(&tmp_dir, workspace.base_dir()).await?;
                    }
                    Err(err)?;
                }
            }
            if need_tmp {
                fs_ext::clean_dir(tmp_dir).await?
            }
            Ok(())
        }
    }
}

#[derive(Default)]
pub struct InstallRunnerProvider {
    mult_progress: MultiProgress,
}

impl InstallRunnerProvider {
    pub async fn make_cargo_runner(
        &self,
        cargo_installer: &CargoInstallerFactory,
        cargo_config: &CargoConfig,
    ) -> Result<Arc<dyn InstallRunner>> {
        let install_targets = cargo_config
            .installs()
            .iter()
            .map(|(name, install_dependency)| {
                CargoInstallTarget::new(name.into(), install_dependency.clone())
            })
            .collect::<Vec<_>>();
        self.make_runner(cargo_installer, install_targets).await
    }

    async fn make_runner<IF: providers::InstallerFactory>(
        &self,
        installer_factory: &IF,
        targets: Vec<IF::InstallTarget>,
    ) -> Result<Arc<dyn InstallRunner>> {
        let core_installer = installer_factory.create_core_installer().await?;
        let bin_path_installer = installer_factory.create_bin_path_installer().await?;
        Ok(Arc::new(InstallRunnerImpl::new(
            core_installer,
            bin_path_installer,
            targets,
            self.mult_progress.clone(),
        )))
    }
}

#[async_trait]
pub trait InstallRunner: 'static + Sync + Send {
    fn provider_type(&self) -> providers::ProviderKind;
    async fn run_installs(&self) -> Result<()>;
    async fn bin_paths(&self) -> Result<Vec<PathBuf>>;
    async fn install_bin_path(&self) -> Result<()>;
}

#[derive(new)]
struct InstallRunnerImpl<
    IT: providers::InstallTarget,
    CI: providers::CoreInstaller<InstallTarget = IT>,
    BI: providers::BinPathInstaller<InstallTarget = IT>,
> {
    core_installer: CI,
    bin_path_installer: BI,
    targets: Vec<IT>,
    mult_progress: MultiProgress,
}

impl<
        IT: providers::InstallTarget,
        CI: providers::CoreInstaller<InstallTarget = IT>,
        BI: providers::BinPathInstaller<InstallTarget = IT>,
    > InstallRunnerImpl<IT, CI, BI>
{
    async fn run_sequential_installs(&self) -> Result<()> {
        for target in self.targets.iter() {
            self.install(target).await?;
        }
        Ok(())
    }
    async fn run_parallel_installs(&self) -> Result<()> {
        join_futures!(self.targets.iter().map(|target| { self.install(target) }))
            .await
            .map_err(InstallServiceError::MultiInstall)?;
        Ok(())
    }
    async fn install(&self, install_target: &IT) -> Result<()> {
        let progress = self.mult_progress.make_progress(install_target);
        progress.start()?;
        match self.core_installer.install(install_target).await {
            Ok(_) => {
                progress.done()?;
                Ok(())
            }
            Err(err) => {
                progress.failed()?;
                Err(err)
            }
        }
    }
}

#[async_trait]
impl<
        IT: providers::InstallTarget,
        CI: providers::CoreInstaller<InstallTarget = IT>,
        BI: providers::BinPathInstaller<InstallTarget = IT>,
    > InstallRunner for InstallRunnerImpl<IT, CI, BI>
{
    fn provider_type(&self) -> providers::ProviderKind {
        self.core_installer.provider_kind()
    }

    async fn run_installs(&self) -> Result<()> {
        match self.core_installer.multi_install_mode() {
            providers::MultiInstallMode::Parallel => self.run_parallel_installs().await,
            providers::MultiInstallMode::Sequential => self.run_sequential_installs().await,
        }
    }
    async fn bin_paths(&self) -> Result<Vec<PathBuf>> {
        let bin_paths = join_futures!(self
            .targets
            .iter()
            .map(|target| self.bin_path_installer.bin_paths(target)))
        .await
        .map_err(InstallServiceError::MultiInstall)?;
        Ok(bin_paths.into_iter().flatten().collect())
    }
    async fn install_bin_path(&self) -> Result<()> {
        join_futures!(self
            .targets
            .iter()
            .map(|target| self.bin_path_installer.install_bin_path(target)))
        .await
        .map_err(InstallServiceError::MultiInstall)?;
        Ok(())
    }
}

#[derive(Getters)]
pub struct InstallServiceOption {
    mode: InstallMode,
}

pub struct InstallServiceOptionBuilder {
    mode: Option<InstallMode>,
}

impl InstallServiceOptionBuilder {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self { mode: None }
    }
    pub fn mode(self, mode: InstallMode) -> Self {
        InstallServiceOptionBuilder { mode: Some(mode) }
    }
    pub fn build(self) -> InstallServiceOption {
        InstallServiceOption {
            mode: self.mode.unwrap_or(InstallMode::All),
        }
    }
}

#[derive(thiserror::Error, Debug, new)]
pub enum InstallServiceError {
    #[error("{0:#?}")]
    MultiInstall(Vec<Error>),

    #[error("{provider}/{name}:\n{error_message}")]
    Install {
        provider: String,
        name: String,
        error_message: String,
        error: Error,
    },

    #[error("duplicate bins:\n{0:#?}")]
    DuplicateBin(Vec<String>),
}
