#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]
#![feature(generic_const_exprs, async_closure, if_let_guard)]

use std::collections::{hash_map::Entry, BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::{net::SocketAddr, sync::Arc};

use axum::{http::StatusCode, routing, Json, Router};
use ffxiv_crafting::{
    Actions, Attributes, CastActionError, Condition, ConditionIterator, Recipe, Status,
};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use rand::{random, seq::SliceRandom, thread_rng};
use sea_orm::{entity::*, query::*, Database, DatabaseConnection, FromQueryResult};
use serde::{Deserialize, Serialize};
use tauri::Manager;
use tokio::sync::{oneshot, Mutex, OnceCell};

mod db;
mod hard_recipe;
mod ordinary_solver;
mod preprogress_solver;
mod rika_solver;
mod solver;

use crate::ordinary_solver::{ProgressSolver, QualitySolver};
use crate::preprogress_solver::PreprogressSolver;
use crate::solver::Solver;
use db::{craft_types, item_with_amount, items, prelude::*, recipes};

/// 创建新的Recipe对象，蕴含了模拟一次制作过程所必要的全部配方信息
#[tauri::command(async)]
fn new_recipe(
    rlv: i32,
    difficulty_factor: u16,
    quality_factor: u16,
    durability_factor: u16,
) -> Recipe {
    Recipe::new(rlv, difficulty_factor, quality_factor, durability_factor)
}

/// 初始化一个表示一次制作的初始状态的Status对象，包含玩家属性、配方信息和初期品质
#[tauri::command(async)]
fn new_status(attrs: Attributes, recipe: Recipe) -> Result<Status, String> {
    if recipe.job_level > attrs.level + 5 {
        Err("Player level lower than recipe's require".to_string())
    } else {
        Ok(Status::new(attrs, recipe))
    }
}

#[derive(Serialize)]
struct CastErrorPos {
    pos: usize,
    err: CastActionError,
}

#[derive(Serialize)]
struct SimulateResult {
    status: Status,
    errors: Vec<CastErrorPos>,
}

/// 模拟以指定初始状态按顺序执行一个技能序列后的结果，
/// 返回值`SimulateResult`包含了最终状态以及模拟过程中每个技能失败的位置及原因
#[tauri::command(async)]
fn simulate(status: Status, actions: Vec<Actions>) -> SimulateResult {
    let mut result = SimulateResult {
        status,
        errors: Vec::new(),
    };
    for (pos, sk) in actions.iter().enumerate() {
        match result.status.is_action_allowed(*sk) {
            Ok(_) => result.status.cast_action(*sk),
            Err(err) => result.errors.push(CastErrorPos { pos, err }),
        }
    }
    result
}

#[derive(Serialize)]
struct SimulateOneStepResult {
    status: Status,
    is_success: bool,
}

/// 只模拟一步制作，计算技能概率和制作状态，更新制作状态
#[tauri::command(async)]
fn simulate_one_step(
    status: Status,
    action: Actions,
    force_success: bool,
) -> Result<SimulateOneStepResult, String> {
    let mut rng = thread_rng();
    let mut status = status.clone();
    status.is_action_allowed(action).map_err(err_to_string)?;
    let is_success = force_success || status.success_rate(action) as f32 / 100.0 > random();
    if is_success {
        status.cast_action(action);
    } else {
        status.cast_action(match action {
            Actions::RapidSynthesis => Actions::RapidSynthesisFail,
            Actions::HastyTouch => Actions::HastyTouchFail,
            Actions::FocusedSynthesis => Actions::FocusedSynthesisFail,
            Actions::FocusedTouch => Actions::FocusedTouchFail,
            _ => unreachable!(),
        });
    }
    if !matches!(action, Actions::FinalAppraisal | Actions::HeartAndSoul) {
        status.condition = if force_success {
            Condition::Normal
        } else {
            ConditionIterator::new(
                status.recipe.conditions_flag as i32,
                status.attributes.level as i32,
            )
            .collect::<Vec<_>>()
            .choose_weighted(&mut rng, |c| c.1)
            .unwrap()
            .0
        };
    }
    Ok(SimulateOneStepResult { status, is_success })
}

#[tauri::command(async)]
fn suggess_next(status: Status) -> Result<Actions, String> {
    use hard_recipe::{LycorisSanguinea, Solver};
    match status {
        s if s.recipe.rlv == 611 => {
            let (str, result) = LycorisSanguinea::run(&s);
            println!("{str} {:?}", result);
            Ok(result
                .get(0)
                .ok_or(String::from("result is empty"))?
                .clone())
        }
        _ => Err(String::from("unsupport recipe")),
    }
}

/// 计算当前状态下可以释放技能的集合，用于模拟界面将不可释放技能置灰
#[tauri::command(async)]
fn allowed_list(status: Status, skills: Vec<Actions>) -> Vec<String> {
    skills
        .iter()
        .map(|&sk| match status.is_action_allowed(sk) {
            Ok(_) => "ok".to_string(),
            Err(err) => err.to_string(),
        })
        .collect()
}

/// 计算当前状态下，传入的技能列表中每个技能需要消耗的制作力，
/// 技能消耗的制作力会受到技能连击和当前制作状态（球色—）的影响
#[tauri::command(async)]
fn craftpoints_list(status: Status, skills: Vec<Actions>) -> Vec<i32> {
    skills.iter().map(|&sk| status.craft_point(sk)).collect()
}

fn err_to_string<T: ToString>(v: T) -> String {
    v.to_string()
}

#[derive(FromQueryResult, Serialize)]
struct RecipeRow {
    id: i32,
    rlv: i32,
    item_id: i32,
    item_name: String,
    job: String,
    difficulty_factor: u16,
    quality_factor: u16,
    durability_factor: u16,
}

#[tauri::command(async)]
async fn recipe_table(
    page_id: u64,
    search_name: String,
    app_state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<(Vec<RecipeRow>, u64), String> {
    let db = app_state.get_db(app_handle).await.map_err(err_to_string)?;
    let paginate = Recipes::find()
        .join(JoinType::InnerJoin, recipes::Relation::CraftTypes.def())
        .join(JoinType::InnerJoin, recipes::Relation::ItemWithAmount.def())
        .join(JoinType::InnerJoin, item_with_amount::Relation::Items.def())
        .filter(items::Column::Name.like(&search_name))
        .column_as(recipes::Column::Id, "id")
        .column_as(recipes::Column::RecipeLevel, "rlv")
        .column_as(recipes::Column::DifficultyFactor, "difficulty_factor")
        .column_as(recipes::Column::QualityFactor, "quality_factor")
        .column_as(recipes::Column::DurabilityFactor, "durability_factor")
        .column_as(craft_types::Column::Name, "job")
        .column_as(items::Column::Id, "item_id")
        .column_as(items::Column::Name, "item_name")
        .into_model::<RecipeRow>()
        .paginate(db, 200);
    let p = paginate.num_pages().await.map_err(err_to_string)?;
    let data = paginate.fetch_page(page_id).await.map_err(err_to_string)?;
    Ok((data, p))
}

#[tauri::command(async)]
async fn recipes_ingredientions(
    checklist: Vec<(i32, i32)>,
    app_state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<Vec<(i32, i32)>, String> {
    let db = app_state.get_db(app_handle).await?;
    let mut needs = BTreeMap::new();
    for (item_id, amount) in checklist {
        let r = Recipes::find()
            .join(JoinType::InnerJoin, recipes::Relation::ItemWithAmount.def())
            .filter(item_with_amount::Column::IngredientId.eq(item_id))
            .one(db)
            .await
            .map_err(err_to_string)?;
        match r {
            Some(r) => {
                let ing = ItemWithAmount::find()
                    .filter(item_with_amount::Column::RecipeId.eq(r.id))
                    .all(db)
                    .await
                    .map_err(err_to_string)?;
                for v in &ing {
                    needs
                        .entry(v.ingredient_id)
                        .and_modify(|e| *e += v.amount)
                        .or_insert(v.amount * amount);
                }
            }
            None => {
                needs
                    .entry(item_id)
                    .and_modify(|e| *e += amount)
                    .or_insert(amount);
            }
        }
    }
    Ok(needs.into_iter().collect())
}

#[tauri::command(async)]
async fn item_info(
    item_id: i32,
    app_state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<items::Model, String> {
    let db = app_state.get_db(app_handle).await?;
    Ok(Items::find_by_id(item_id)
        .one(db)
        .await
        .map_err(err_to_string)?
        .ok_or("Item not found".to_string())?)
}

struct AppState {
    solver_list: Mutex<HashMap<ordinary_solver::SolverHash, Option<Box<dyn Solver + Send + Sync>>>>,
    db: OnceCell<DatabaseConnection>,
    shutdown_signal: Mutex<Option<oneshot::Sender<()>>>,
    should_be_transparent: AtomicBool,
}

impl AppState {
    fn new() -> Self {
        Self {
            solver_list: Mutex::new(HashMap::new()),
            db: OnceCell::new(),
            shutdown_signal: Mutex::new(None),
            should_be_transparent: AtomicBool::new(false),
        }
    }

    async fn get_db(&self, app_handle: tauri::AppHandle) -> Result<&DatabaseConnection, String> {
        const ESCAPE_SET: &AsciiSet = &CONTROLS.add(b'?').add(b'#');
        let path = app_handle
            .path_resolver()
            .resolve_resource("assets/xiv.db")
            .ok_or("database file not found")?;
        let path = utf8_percent_encode(&path.to_string_lossy(), ESCAPE_SET).to_string();
        self.db
            .get_or_try_init(|| Database::connect(format!("sqlite:{}?mode=ro", path)))
            .await
            .map_err(err_to_string)
    }
}

#[tauri::command(async)]
async fn create_solver(
    status: Status,
    use_muscle_memory: bool,
    use_manipulation: bool,
    app_state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let key = ordinary_solver::SolverHash {
        attributes: status.attributes,
        recipe: status.recipe,
    };
    let slot = {
        let mut list = app_state.solver_list.lock().await;
        match list.entry(key.clone()) {
            Entry::Occupied(e) => match e.get() {
                Some(_) => Err("solver already exists".to_string()),
                None => Err("solver is creating".to_string()),
            },
            Entry::Vacant(e) => {
                e.insert(None); // tell others we are already take this place
                Ok(())
            }
        }
    };
    if let Ok(()) = slot {
        macro_rules! build_solver {
            ($mn:literal, $wn:literal, $pl:expr) => {{
                let mut driver = ProgressSolver::new(status.clone());
                driver.init();
                let mut solver = PreprogressSolver::<$mn, $wn>::new(status, $pl, Arc::new(driver));
                solver.init();
                Box::new(solver)
            }};
            ($mn:literal, $wn:literal) => {{
                let mut driver = ProgressSolver::new(status.clone());
                driver.init();
                let mut solver = QualitySolver::<$mn, $wn>::new(status, Arc::new(driver));
                solver.init();
                Box::new(solver)
            }};
        }
        let progress_list = preprogress_list(&status);
        let solver: Box<dyn Solver + Send + Sync> = match (use_muscle_memory, use_manipulation) {
            (true, true) => build_solver!(8, 8, progress_list),
            (true, false) => build_solver!(0, 8, progress_list),
            (false, true) => build_solver!(8, 8),
            (false, false) => build_solver!(0, 8),
        };
        let mut list = app_state.solver_list.lock().await;
        *list.get_mut(&key).unwrap() = Some(solver); // we are sure that there is a None value so we can successfully get it
        Ok(())
    } else {
        slot
    }
}

fn preprogress_list(status: &Status) -> Vec<u16> {
    macro_rules! level_based {
        ($level:literal, $e1:literal, $e2:literal) => {
            if status.attributes.level < $level {
                $e1
            } else {
                $e2
            }
        };
    }
    vec![
        status.calc_synthesis(level_based!(31, 1.0, 1.2)), // basic synth
        status.calc_synthesis(level_based!(82, 1.5, 1.8)), // careful synth
    ]
}

/// 调用求解器
#[tauri::command(async)]
async fn read_solver(
    status: Status,
    app_state: tauri::State<'_, AppState>,
) -> Result<Vec<Actions>, String> {
    let key = ordinary_solver::SolverHash {
        attributes: status.attributes,
        recipe: status.recipe,
    };
    let mut list = app_state.solver_list.lock().await;
    match list.entry(key) {
        Entry::Occupied(e) => {
            if let Some(v) = e.get() {
                Ok(v.read_all(&status))
            } else {
                Err("solver not prepared".to_string())
            }
        }
        _ => Err("solver not exists".to_string()),
    }
}

#[tauri::command(async)]
fn rika_solve(status: Status) -> Vec<Actions> {
    rika_solver::solve(status)
}

/// 释放求解器
#[tauri::command(async)]
async fn destroy_solver(
    status: Status,
    app_state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let key = ordinary_solver::SolverHash {
        attributes: status.attributes,
        recipe: status.recipe,
    };
    let mut list = app_state.solver_list.lock().await;
    match list.entry(key) {
        Entry::Occupied(e) => match e.get() {
            Some(_) => {
                e.remove();
                Ok(())
            }
            None => Err("solver is creating".to_string()), // we can't take the solver when it is a None, because the creating thread expect it will not be remove
        },
        Entry::Vacant(_) => Err("solver not exists".to_string()),
    }
}

#[tauri::command]
fn should_be_transparent(app_state: tauri::State<AppState>) -> bool {
    app_state.should_be_transparent.load(Ordering::SeqCst)
}

#[tauri::command(async)]
async fn start_http_server(
    addr: String,
    app_state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    #[derive(Deserialize, Clone, Debug)]
    struct ActionStep {
        action: i32,
        progress: u16,
        quality: u32,
        durability: u16,
        condition: i8,
    }
    #[derive(Serialize, Clone)]
    struct ActionStepJS {
        action: Actions,
        progress: u16,
        quality: u32,
        durability: u16,
        condition: i8,
    }
    let action = async move |Json(payload): Json<ActionStep>| {
        println!("{:?}", payload);
        let Some(sk) = ffxiv_crafting::data::action_table(payload.action) else {
            return (StatusCode::BAD_REQUEST, ())
        };
        app_handle
            .emit_all(
                "action-step",
                ActionStepJS {
                    action: sk,
                    progress: payload.progress,
                    quality: payload.quality,
                    durability: payload.durability,
                    condition: payload.condition,
                },
            )
            .unwrap();
        (StatusCode::NO_CONTENT, ())
    };

    println!("starting http server");
    let rx = {
        let mut current_tx = app_state.shutdown_signal.lock().await;
        if let Some(_) = *current_tx {
            return Err(String::from("http server is running"));
        }
        let (tx, rx) = oneshot::channel::<()>();
        *current_tx = Some(tx);
        rx
    };
    let server = Router::new().route("/action", routing::post(action));
    let addr: SocketAddr = addr.parse().unwrap();
    axum::Server::bind(&addr)
        .serve(server.into_make_service())
        .with_graceful_shutdown(wait_for_shutdown(rx))
        .await
        .unwrap();
    Ok(())
}

#[allow(unused_must_use)]
async fn wait_for_shutdown(rx: oneshot::Receiver<()>) {
    rx.await;
}

fn main() {
    tauri::Builder::default()
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            new_recipe,
            new_status,
            simulate,
            simulate_one_step,
            suggess_next,
            allowed_list,
            craftpoints_list,
            recipe_table,
            recipes_ingredientions,
            item_info,
            create_solver,
            read_solver,
            destroy_solver,
            rika_solve,
            should_be_transparent,
            start_http_server,
        ])
        .setup(|app| {
            let window = app.get_window("main").unwrap();
            window.set_decorations(true)?;

            let state = app.state::<AppState>();

            #[allow(unused_mut, unused_assignments)]
            let mut sbt = false;
            #[cfg(target_os = "macos")]
            {
                use window_vibrancy::NSVisualEffectMaterial;
                sbt = window_vibrancy::apply_vibrancy(
                    &window,
                    NSVisualEffectMaterial::HudWindow,
                    None,
                    None,
                )
                .is_ok();
            }

            #[cfg(target_os = "windows")]
            {
                sbt = window_vibrancy::apply_mica(&window).is_ok();
            }

            state
                .inner()
                .should_be_transparent
                .store(sbt, Ordering::SeqCst);

            Ok(())
        })
        .run(tauri::generate_context!())
        .map_err(|err| msgbox::create("Error", err.to_string().as_str(), msgbox::IconType::Error))
        .unwrap();
}
