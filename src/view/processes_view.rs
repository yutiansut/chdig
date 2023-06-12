use std::cmp::Ordering;
use std::collections::HashMap;
use std::mem::take;

use cursive::traits::{Nameable, Resizable};
use cursive::{
    event::{Event, EventResult},
    inner_getters,
    view::ScrollStrategy,
    view::ViewWrapper,
    views, Cursive,
};
use size::{Base, SizeFormatter, Style};

use crate::interpreter::{
    clickhouse::Columns, clickhouse::TraceType, options::ViewOptions, BackgroundRunner, ContextArc,
    QueryProcess, WorkerEvent,
};
use crate::view::{ExtTableView, ProcessView, QueryResultView, TableViewItem, TextLogView};
use crate::wrap_impl_no_move;

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub enum QueryProcessesColumn {
    HostName,
    SubQueries,
    Cpu,
    User,
    Threads,
    Memory,
    DiskIO,
    NetIO,
    Elapsed,
    QueryId,
    Query,
}
impl PartialEq<QueryProcess> for QueryProcess {
    fn eq(&self, other: &Self) -> bool {
        return self.query_id == other.query_id;
    }
}

impl TableViewItem<QueryProcessesColumn> for QueryProcess {
    fn to_column(&self, column: QueryProcessesColumn) -> String {
        let formatter = SizeFormatter::new()
            .with_base(Base::Base2)
            .with_style(Style::Abbreviated);

        match column {
            QueryProcessesColumn::HostName => self.host_name.to_string(),
            QueryProcessesColumn::SubQueries => {
                if self.is_initial_query {
                    return self.subqueries.to_string();
                } else {
                    return 1.to_string();
                }
            }
            QueryProcessesColumn::Cpu => format!("{:.1} %", self.cpu()),
            QueryProcessesColumn::User => self.user.clone(),
            QueryProcessesColumn::Threads => self.threads.to_string(),
            QueryProcessesColumn::Memory => formatter.format(self.memory),
            QueryProcessesColumn::DiskIO => formatter.format(self.disk_io() as i64),
            QueryProcessesColumn::NetIO => formatter.format(self.net_io() as i64),
            QueryProcessesColumn::Elapsed => format!("{:.2}", self.elapsed),
            QueryProcessesColumn::QueryId => {
                if self.subqueries > 1 && self.is_initial_query {
                    return format!("-> {}", self.query_id);
                } else {
                    return self.query_id.clone();
                }
            }
            QueryProcessesColumn::Query => self.normalized_query.clone(),
        }
    }

    fn cmp(&self, other: &Self, column: QueryProcessesColumn) -> Ordering
    where
        Self: Sized,
    {
        match column {
            QueryProcessesColumn::HostName => self.host_name.cmp(&other.host_name),
            QueryProcessesColumn::SubQueries => self.subqueries.cmp(&other.subqueries),
            QueryProcessesColumn::Cpu => self.cpu().total_cmp(&other.cpu()),
            QueryProcessesColumn::User => self.user.cmp(&other.user),
            QueryProcessesColumn::Threads => self.threads.cmp(&other.threads),
            QueryProcessesColumn::Memory => self.memory.cmp(&other.memory),
            QueryProcessesColumn::DiskIO => self.disk_io().total_cmp(&other.disk_io()),
            QueryProcessesColumn::NetIO => self.net_io().total_cmp(&other.net_io()),
            QueryProcessesColumn::Elapsed => self.elapsed.total_cmp(&other.elapsed),
            QueryProcessesColumn::QueryId => self.query_id.cmp(&other.query_id),
            QueryProcessesColumn::Query => self.normalized_query.cmp(&other.normalized_query),
        }
    }
}

pub struct ProcessesView {
    context: ContextArc,
    table: ExtTableView<QueryProcess, QueryProcessesColumn>,
    items: HashMap<String, QueryProcess>,
    query_id: Option<String>,
    options: ViewOptions,

    #[allow(unused)]
    bg_runner: BackgroundRunner,
}

impl ProcessesView {
    inner_getters!(self.table: ExtTableView<QueryProcess, QueryProcessesColumn>);

    pub fn update(self: &mut Self, processes: Columns) {
        let prev_items = take(&mut self.items);

        // TODO: write some closure to extract the field with type propagation.
        for i in 0..processes.row_count() {
            let mut query_process = QueryProcess {
                host_name: processes.get::<_, _>(i, "host_name").expect("host_name"),
                user: processes.get::<_, _>(i, "user").expect("user"),
                threads: processes
                    .get::<Vec<u64>, _>(i, "thread_ids")
                    .expect("thread_ids")
                    .len(),
                memory: processes
                    .get::<_, _>(i, "peak_memory_usage")
                    .expect("peak_memory_usage"),
                elapsed: processes.get::<_, _>(i, "elapsed").expect("elapsed"),
                subqueries: processes.get::<_, _>(i, "subqueries").expect("subqueries"),
                is_initial_query: processes
                    .get::<u8, _>(i, "is_initial_query")
                    .expect("is_initial_query")
                    == 1,
                initial_query_id: processes
                    .get::<_, _>(i, "initial_query_id")
                    .expect("initial_query_id"),
                query_id: processes.get::<_, _>(i, "query_id").expect("query_id"),
                normalized_query: processes
                    .get::<_, _>(i, "normalized_query")
                    .expect("normalizeQuery"),
                original_query: processes
                    .get::<_, _>(i, "original_query")
                    .expect("original_query"),
                profile_events: processes
                    .get::<_, _>(i, "ProfileEvents")
                    .expect("ProfileEvents"),

                prev_elapsed: None,
                prev_profile_events: None,
            };

            // FIXME: Shrinking is slow, but without it memory consumption is too high, 100-200x
            // more! This is because by some reason the capacity inside clickhouse.rs is 4096,
            // which is ~100x more then we need for ProfileEvents (~40).
            query_process.profile_events.shrink_to_fit();

            if let Some(prev_item) = prev_items.get(&query_process.query_id) {
                query_process.prev_elapsed = Some(prev_item.elapsed);
                query_process.prev_profile_events = Some(prev_item.profile_events.clone());
            }

            self.items
                .insert(query_process.query_id.clone(), query_process);
        }

        self.update_view();
    }

    fn update_view(self: &mut Self) {
        let mut items = Vec::new();
        if let Some(query_id) = &self.query_id {
            for (_, query_process) in &self.items {
                if query_process.initial_query_id == *query_id {
                    items.push(query_process.clone());
                }
            }
        } else {
            for (_, query_process) in &self.items {
                if self.options.group_by && !query_process.is_initial_query {
                    continue;
                }
                items.push(query_process.clone());
            }
        }

        let inner_table = self.table.get_inner_mut().get_inner_mut();
        if inner_table.is_empty() {
            inner_table.set_items_stable(items);
            // NOTE: this is not a good solution since in this case we cannot select always first
            // row if user did not select anything...
            inner_table.set_selected_row(0);
        } else {
            inner_table.set_items_stable(items);
        }
    }

    fn show_flamegraph(self: &mut Self, trace_type: Option<TraceType>) {
        let inner_table = self.table.get_inner_mut().get_inner_mut();

        if inner_table.item().is_none() {
            return;
        }

        let mut context_locked = self.context.lock().unwrap();
        let item_index = inner_table.item().unwrap();
        let query_id = inner_table
            .borrow_item(item_index)
            .unwrap()
            .query_id
            .clone();

        let mut query_ids = Vec::new();

        query_ids.push(query_id.clone());
        if !self.options.no_subqueries {
            for (_, query_process) in &self.items {
                if query_process.initial_query_id == *query_id {
                    query_ids.push(query_process.query_id.clone());
                }
            }
        }

        if let Some(trace_type) = trace_type {
            context_locked
                .worker
                .send(WorkerEvent::ShowQueryFlameGraph(trace_type, query_ids));
        } else {
            context_locked
                .worker
                .send(WorkerEvent::ShowLiveQueryFlameGraph(query_ids));
        }
    }

    pub fn new(context: ContextArc, event: WorkerEvent) -> views::OnEventView<Self> {
        let delay = context.lock().unwrap().options.view.delay_interval;

        let update_callback_context = context.clone();
        let update_callback = move || {
            update_callback_context
                .lock()
                .unwrap()
                .worker
                .send(event.clone());
        };

        let mut table = ExtTableView::<QueryProcess, QueryProcessesColumn>::default();
        let inner_table = table.get_inner_mut().get_inner_mut();
        inner_table.add_column(QueryProcessesColumn::QueryId, "query_id", |c| c.width(12));
        inner_table.add_column(QueryProcessesColumn::Cpu, "cpu", |c| c.width(8));
        inner_table.add_column(QueryProcessesColumn::User, "user", |c| c.width(8));
        inner_table.add_column(QueryProcessesColumn::Threads, "thr", |c| c.width(6));
        inner_table.add_column(QueryProcessesColumn::Memory, "mem", |c| c.width(6));
        inner_table.add_column(QueryProcessesColumn::DiskIO, "disk", |c| c.width(7));
        inner_table.add_column(QueryProcessesColumn::NetIO, "net", |c| c.width(6));
        inner_table.add_column(QueryProcessesColumn::Elapsed, "elapsed", |c| c.width(11));
        inner_table.add_column(QueryProcessesColumn::Query, "query", |c| c);
        inner_table.set_on_submit(|siv: &mut Cursive, _row: usize, _index: usize| {
            siv.on_event(Event::Char('l'));
        });

        inner_table.sort_by(QueryProcessesColumn::Elapsed, Ordering::Greater);

        let view_options = context.lock().unwrap().options.view.clone();

        if !view_options.no_subqueries {
            inner_table.insert_column(0, QueryProcessesColumn::SubQueries, "Q#", |c| c.width(5));
        }
        if context.lock().unwrap().options.clickhouse.cluster.is_some() {
            inner_table.insert_column(0, QueryProcessesColumn::HostName, "host", |c| c.width(8));
        }

        let mut bg_runner = BackgroundRunner::new(delay);
        bg_runner.start(update_callback);

        let processes_view = ProcessesView {
            context: context.clone(),
            table,
            items: HashMap::new(),
            query_id: None,
            options: view_options,
            bg_runner,
        };

        // TODO:
        // - pause/disable the table if the foreground view had been changed
        // - space - multiquery selection (KILL, flamegraphs, logs, ...)
        let mut event_view = views::OnEventView::new(processes_view);

        let context_copy = context.clone();
        event_view.set_on_pre_event_inner(Event::Refresh, move |v, _| {
            let action_callback = context_copy.lock().unwrap().pending_view_callback.take();
            if let Some(action_callback) = action_callback {
                action_callback.as_ref()(v);
                return Some(EventResult::consumed());
            }
            return Some(EventResult::Ignored);
        });

        let mut context = context.lock().unwrap();
        context.add_view_action(&mut event_view, "Show all queries", Event::Char('-'), |v| {
            let v = v.downcast_mut::<ProcessesView>().unwrap();
            v.query_id = None;
            v.update_view();
        });
        context.add_view_action(
            &mut event_view,
            "Show queries on shards",
            Event::Char('+'),
            |v| {
                let v = v.downcast_mut::<ProcessesView>().unwrap();
                let inner_table = v.table.get_inner_mut().get_inner_mut();

                if inner_table.item().is_none() {
                    return;
                }

                let item_index = inner_table.item().unwrap();
                let query_id = inner_table
                    .borrow_item(item_index)
                    .unwrap()
                    .query_id
                    .clone();

                v.query_id = Some(query_id);
                v.update_view();
            },
        );
        context.add_view_action(&mut event_view, "Query details", Event::Char('D'), |v| {
            let v = v.downcast_mut::<ProcessesView>().unwrap();
            let inner_table = v.table.get_inner_mut().get_inner_mut();

            if inner_table.item().is_none() {
                return;
            }

            let item_index = inner_table.item().unwrap();
            let row = inner_table.borrow_item(item_index).unwrap().clone();

            v.context
                .lock()
                .unwrap()
                .cb_sink
                .send(Box::new(move |siv: &mut cursive::Cursive| {
                    siv.add_layer(views::Dialog::around(
                        ProcessView::new(row)
                            .with_name("process")
                            .min_size((70, 35)),
                    ));
                }))
                .unwrap();
        });
        context.add_view_action(&mut event_view, "Query processors", Event::Char('P'), |v| {
            let v = v.downcast_mut::<ProcessesView>().unwrap();
            let inner_table = v.table.get_inner_mut().get_inner_mut();

            if inner_table.item().is_none() {
                return;
            }

            let item_index = inner_table.item().unwrap();
            let item = inner_table.borrow_item(item_index).unwrap();

            // NOTE: Even though we request for all queries, we may not have any child
            // queries already, so for better picture we need to combine results from
            // system.processors_profile_log
            //
            // FIXME: after [1] we could simply use "initial_query_id"
            //
            //   [1]: https://github.com/ClickHouse/ClickHouse/pull/49777
            let query_id = item.query_id.clone();
            let mut query_ids = Vec::new();
            query_ids.push(query_id.clone());
            if !v.options.no_subqueries {
                for (_, query_process) in &v.items {
                    if query_process.initial_query_id == *query_id {
                        query_ids.push(query_process.query_id.clone());
                    }
                }
            }

            let columns = vec![
                "name",
                "count() count",
                // TODO: support this units in QueryResultView
                "sum(elapsed_us)/1e6 elapsed_sec",
                "sum(input_wait_elapsed_us)/1e6 input_wait_sec",
                "sum(output_wait_elapsed_us)/1e6 output_wait_sec",
                "sum(input_rows) rows",
                "sum(input_bytes) bytes",
                "round(bytes/elapsed_sec,2)/1e6 bytes_per_sec",
            ];
            let sort_by = "elapsed_sec";
            let table = "system.processors_profile_log";
            let dbtable = v.context.lock().unwrap().clickhouse.get_table_name(table);
            let query = format!(
                r#"SELECT {}
                FROM {}
                WHERE
                    // TODO: extract from the query
                    event_date >= yesterday() AND
                    query_id IN ('{}')
                GROUP BY name
                ORDER BY name ASC
                "#,
                columns.join(", "),
                dbtable,
                query_ids.join("','"),
            );

            let context_copy = v.context.clone();
            v.context
                .lock()
                .unwrap()
                .cb_sink
                .send(Box::new(move |siv: &mut cursive::Cursive| {
                    siv.add_layer(views::Dialog::around(
                        views::LinearLayout::vertical()
                            .child(views::TextView::new("Processors:").center())
                            .child(views::DummyView.fixed_height(1))
                            .child(
                                QueryResultView::new(
                                    context_copy,
                                    table,
                                    sort_by,
                                    columns.clone(),
                                    query,
                                )
                                .expect(&format!("Cannot get {}", table))
                                .with_name(table)
                                // TODO: autocalculate
                                .min_size((160, 40)),
                            ),
                    ));
                }))
                .unwrap();
        });
        context.add_view_action(&mut event_view, "Query views", Event::Char('v'), |v| {
            let v = v.downcast_mut::<ProcessesView>().unwrap();
            let inner_table = v.table.get_inner_mut().get_inner_mut();

            if inner_table.item().is_none() {
                return;
            }

            let item_index = inner_table.item().unwrap();
            let item = inner_table.borrow_item(item_index).unwrap();

            // NOTE: Even though we request for all queries, we may not have any child
            // queries already, so for better picture we need to combine results from
            // system.query_views_log
            let query_id = item.query_id.clone();
            let mut query_ids = Vec::new();
            query_ids.push(query_id.clone());
            if !v.options.no_subqueries {
                for (_, query_process) in &v.items {
                    if query_process.initial_query_id == *query_id {
                        query_ids.push(query_process.query_id.clone());
                    }
                }
            }

            let columns = vec!["view_name", "view_duration_ms"];
            let sort_by = "view_duration_ms";
            let table = "system.query_views_log";
            let dbtable = v.context.lock().unwrap().clickhouse.get_table_name(table);
            let query = format!(
                r#"SELECT {}
                FROM {}
                WHERE
                    // TODO: extract from the query
                    event_date >= yesterday() AND
                    initial_query_id IN ('{}')
                ORDER BY view_duration_ms DESC
                "#,
                columns.join(", "),
                dbtable,
                query_ids.join("','"),
            );

            let context_copy = v.context.clone();
            v.context
                .lock()
                .unwrap()
                .cb_sink
                .send(Box::new(move |siv: &mut cursive::Cursive| {
                    siv.add_layer(views::Dialog::around(
                        views::LinearLayout::vertical()
                            .child(views::TextView::new("Views:").center())
                            .child(views::DummyView.fixed_height(1))
                            .child(
                                QueryResultView::new(
                                    context_copy,
                                    table,
                                    sort_by,
                                    columns.clone(),
                                    query,
                                )
                                .expect(&format!("Cannot get {}", table))
                                .with_name(table)
                                // TODO: autocalculate
                                .min_size((160, 40)),
                            ),
                    ));
                }))
                .unwrap();
        });
        context.add_view_action(
            &mut event_view,
            "Show CPU flamegraph",
            Event::Char('C'),
            |v| {
                let v = v.downcast_mut::<ProcessesView>().unwrap();
                v.show_flamegraph(Some(TraceType::CPU));
            },
        );
        context.add_view_action(
            &mut event_view,
            "Show Real flamegraph",
            Event::Char('R'),
            |v| {
                let v = v.downcast_mut::<ProcessesView>().unwrap();
                v.show_flamegraph(Some(TraceType::Real));
            },
        );
        context.add_view_action(
            &mut event_view,
            "Show memory flamegraph",
            Event::Char('M'),
            |v| {
                let v = v.downcast_mut::<ProcessesView>().unwrap();
                v.show_flamegraph(Some(TraceType::Memory));
            },
        );
        context.add_view_action(
            &mut event_view,
            "Show live flamegraph",
            Event::Char('L'),
            |v| {
                let v = v.downcast_mut::<ProcessesView>().unwrap();
                v.show_flamegraph(None);
            },
        );
        context.add_view_action(&mut event_view, "EXPLAIN PLAN", Event::Char('e'), |v| {
            let v = v.downcast_mut::<ProcessesView>().unwrap();
            let inner_table = v.table.get_inner_mut().get_inner_mut();

            if inner_table.item().is_none() {
                return;
            }

            let mut context_locked = v.context.lock().unwrap();
            let item_index = inner_table.item().unwrap();
            let query = inner_table
                .borrow_item(item_index)
                .unwrap()
                .original_query
                .clone();
            context_locked.worker.send(WorkerEvent::ExplainPlan(query));
        });
        context.add_view_action(&mut event_view, "EXPLAIN PIPELINE", Event::Char('E'), |v| {
            let v = v.downcast_mut::<ProcessesView>().unwrap();
            let inner_table = v.table.get_inner_mut().get_inner_mut();

            if inner_table.item().is_none() {
                return;
            }

            let mut context_locked = v.context.lock().unwrap();
            let item_index = inner_table.item().unwrap();
            let query = inner_table
                .borrow_item(item_index)
                .unwrap()
                .original_query
                .clone();
            context_locked
                .worker
                .send(WorkerEvent::ExplainPipeline(query));
        });
        context.add_view_action(&mut event_view, "KILL query", Event::Char('K'), |v| {
            let v = v.downcast_mut::<ProcessesView>().unwrap();
            let inner_table = v.table.get_inner_mut().get_inner_mut();

            if inner_table.item().is_none() {
                return;
            }

            let item_index = inner_table.item().unwrap();
            let query_id = inner_table
                .borrow_item(item_index)
                .unwrap()
                .query_id
                .clone();
            let context_copy = v.context.clone();

            v.context
                .lock()
                .unwrap()
                .cb_sink
                .send(Box::new(move |siv: &mut cursive::Cursive| {
                    siv.add_layer(
                        views::Dialog::new()
                            .title(&format!(
                                "Are you sure you want to KILL QUERY with query_id = {}",
                                query_id
                            ))
                            .button("Yes, I'm sure", move |s| {
                                context_copy
                                    .lock()
                                    .unwrap()
                                    .worker
                                    .send(WorkerEvent::KillQuery(query_id.clone()));
                                // TODO: wait for the KILL
                                s.pop_layer();
                            })
                            .button("Cancel", |s| {
                                s.pop_layer();
                            }),
                    );
                }))
                .unwrap();
        });
        context.add_view_action(&mut event_view, "Show query logs", Event::Char('l'), |v| {
            let v = v.downcast_mut::<ProcessesView>().unwrap();
            let inner_table = v.table.get_inner_mut().get_inner_mut();

            if inner_table.item().is_none() {
                return;
            }

            let item_index = inner_table.item().unwrap();
            let item = inner_table.borrow_item(item_index).unwrap();

            // NOTE: Even though we request logs for all queries, we may not have any child
            // queries already, so for better picture we need to combine results from
            // system.query_log
            let query_id = item.query_id.clone();
            let mut query_ids = Vec::new();
            query_ids.push(query_id.clone());
            if !v.options.no_subqueries {
                for (_, query_process) in &v.items {
                    if query_process.initial_query_id == *query_id {
                        query_ids.push(query_process.query_id.clone());
                    }
                }
            }

            let context_copy = v.context.clone();
            v.context
                .lock()
                .unwrap()
                .cb_sink
                .send(Box::new(move |siv: &mut cursive::Cursive| {
                    siv.add_layer(views::Dialog::around(
                        views::LinearLayout::vertical()
                            .child(views::TextView::new("Logs:").center())
                            .child(views::DummyView.fixed_height(1))
                            .child(
                                views::ScrollView::new(views::NamedView::new(
                                    "query_log",
                                    TextLogView::new(context_copy, query_ids),
                                ))
                                .scroll_strategy(ScrollStrategy::StickToBottom)
                                .scroll_x(true),
                            ),
                    ));
                }))
                .unwrap();
        });
        return event_view;
    }
}

impl Drop for ProcessesView {
    fn drop(&mut self) {
        log::debug!("Removing views actions");
        self.context.lock().unwrap().view_actions.clear();
    }
}

// TODO: remove this extra wrapping
impl ViewWrapper for ProcessesView {
    wrap_impl_no_move!(self.table: ExtTableView<QueryProcess, QueryProcessesColumn>);
}
