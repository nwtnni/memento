from os.path import exists
import re
import matplotlib.pyplot as plt
import numpy as np
import pandas as pd
import os.path
import git
import shutil

objs = {
    "hash": {
        "targets": {
            # "CCEH": {'label': "CCEH", 'marker': 'x', 'color': 'skyblue', 'style': '-'},
            # 'Level': {'label': "Level", 'marker': 'o', 'color': 'orange', 'style': '-'},
            # 'Dash': {'label': "Dash", 'marker': '^', 'color': 'green', 'style': '-'},
            # 'PCLHT': {'label': "PCLHT", 'marker': 'v', 'color': 'gold', 'style': '-'},
            "clevel": {'label': "Clevel", 'marker': 's', 'color': 'gray', 'style': '-'},
            "clevel_rust": {
                'label': "Clevel-mmt", 'marker': 's', 'color': 'black', 'style': '-',
                'data_id': {  # select data manually. or read latest data
                    'insert': '',
                    'pos_search': '',
                    'neg_search': '',
                    'delete': '',
                    'write_heavy': '',
                    'balanced': '',
                    'read_heavy': '',
                }
            },
            # 'SOFT': {'label': "SOFT", 'marker': 'o', 'color': 'royalblue', 'style': '-'},
            # 'SOFT_rust': {
            #     'label': "SOFT-mmt", 'marker': 'o', 'color': 'blue', 'style': '-',
            #     'data_id': {  # select data manually. or read latest data
            #         'insert': '',
            #         'pos_search': '',
            #         'neg_search': '',
            #         'delete': '',
            #         'write_heavy': '',
            #         'balanced': '',
            #         'read_heavy': '',
            #     }
            # },
        },
        'bench_kinds': {
            'throughput': {
                'workloads': {
                    'insert': {'label': "(a) Insert", 'label_single': "Insert"},
                    'pos_search': {'label': "(b) Pos. Search", 'label_single': "Pos. Search"},
                    'neg_search': {'label': "(c) Neg. Search", 'label_single': "Neg. Search"},
                    'delete': {'label': "(d) Delete", 'label_single': "Delete"},
                    'write_heavy': {'label': "(e) Write heavy"},
                    'balanced': {'label': "(f) Balanced"},
                    'read_heavy': {'label': "(g) Read heavy"},
                },
                'distributions': ['uniform', 'selfsimilar'],

                'x': ['1', '4', '8', '16', '24', '32', '48', '64'],
                'x_label': 'Threads',
                'y_label': 'Throughput (M op/s)',
            },
            # 'latency': {
            #     'workloads': {
            #         'insert': {'label': "(a) Insert"},
            #         'pos_search': {'label': "(b) Pos. Search"},
            #         'neg_search': {'label': "(c) Neg. Search"},
            #         'delete': {'label': "(d) Delete"},
            #     },
            #     'distributions': ['uniform'],

            #     'x': ['0%', '50%', '90%', '99%', '99.9%', '99.99%', '99.999%'],
            #     'x_label': 'Percentile',
            #     'y_label': 'Latency (ns)',
            # },
            # 'load_factor': {
            #     'workloads': {
            #         'insert',
            #     },
            #     'distributions': ['uniform'],

            #     'x': ['0', '50', '100', '150', '200'],
            #     'x_label': 'Number of insertions(M)',
            #     'y_label': 'Load Factor',
            # },
        },
    },
}


def get_filepath(bench, dist, workload, target):
    # print(bench, dist, workload, target)
    if 'data_id' in objs['hash']['targets'][target]:
        data_id = objs['hash']['targets'][target]['data_id'][workload]

        if data_id == '':
            repo = git.Repo(search_parent_directories=True)
            for commit in repo.iter_commits():
                filepath = "./out/{}/{}/{}/{}_{}_{}.out".format(
                    bench.upper(), dist.upper(), workload, target, commit.hexsha[:7], commit.committed_datetime.strftime('%Y%m%d'))
                if exists(filepath):
                    return filepath
        filepath = "./out/{}/{}/{}/{}_{}.out".format(
            bench.upper(), dist.upper(), workload, target, data_id)
    else:
        filepath = "./out/{}/{}/{}/{}.out".format(
            bench.upper(), dist.upper(), workload, target)
        if exists(filepath):
            return filepath
        else:
            repo = git.Repo(search_parent_directories=True)
            for commit in repo.iter_commits():
                filepath = "./out/{}/{}/{}/{}_{}_{}.out".format(
                    bench.upper(), dist.upper(), workload, target, commit.hexsha[:7], commit.committed_datetime.strftime('%Y%m%d'))
                if exists(filepath):
                    return filepath
    return filepath


def read_throughputs(filepath):
    print("read", filepath)
    threads = []
    throughputs = []
    with open(filepath, "r") as f:
        tn = -1
        for i in f.readlines():
            t = re.search('(?<=# Threads: )\d+', i)
            if t:
                threads.append(int(t[0]))
                throughputs.append(None)  # dummy value
                tn += 1
            m = re.search('(?<=Throughput\(Mops/s\): )\d+.\d', i)
            if m:
                throughputs[tn] = float(m[0])
    return threads, throughputs


N_LATENCY = 7 # ['0%', '50%', '90%', '99%', '99.9%', '99.99%', '99.999%']


def read_latency(filepath):
    latency = []
    with open(filepath, "r") as f:
        itf = iter(f)
        for line in itf:
            m = re.search('(?<=Latency\(ns\):)', line)
            if m:
                for i in range(0, N_LATENCY):
                    line = next(itf)  # BEWARE, This could raise StopIteration!
                    ix, lt = line.split()
                    latency.append(int(lt))
    return latency


def draw_legend(line, label, figpath):
    plt.clf()
    legendFig = plt.figure("Legend plot")
    legendFig.legend(line, label, loc='center',
                     ncol=len(line))
    legendFig.savefig(figpath, bbox_inches='tight')
    print(figpath)


latency_label_done = False


def draw_ax(bench, ax, datas):
    for data in datas:
        if bench == "latency":
            ax.plot(data['x'], data['y'], label=data['label'],
                    color=data['color'], linestyle=data['style'], marker=data['marker'], markersize=4)
        else:
            ax.plot(data['x'], data['y'], label=data['label'],
                    color=data['color'], linestyle=data['style'], marker=data['marker'])

    if bench == "latency":
        print(data['xlabel'])
        ax.tick_params(labelrotation=25)
        ax.set_yticks(np.arange(1, 4))
        ax.set_yticklabels(['$10^3$', '$10^6$', '$10^9$'], rotation=0)
        ax.tick_params(axis='x', labelsize=5)
        ax.tick_params(axis='y', labelsize=7)

        global latency_label_done
        if not latency_label_done:
            ax.set_yticklabels(['$10^3$', '$10^6$', '$10^9$'], rotation=0)
            latency_label_done = True
        else:
            ax.set_yticklabels([], rotation=0)
    ax.grid()
    ax.set_xlabel(data['xlabel'], fontsize=10)

    # Make red area
    x_range, y_range = ax.get_xlim(), ax.get_ylim()
    ax.fill_between([6.2, x_range[1]], y_range[0], y_range[1], alpha=0.08, color='red')
    ax.set_xlim(x_range)
    ax.set_ylim(y_range)


def draw_axes(bench, ylabel, axes_datas):
    if bench == 'latency':
        figsize = (6, 0.8)
        print(ylabel)
    else:
        figsize = (10, 1.2)
    fig, axes = plt.subplots(1, len(axes_datas), figsize=figsize)
    for i, ax_datas in enumerate(axes_datas):
        draw_ax(bench, axes[i], ax_datas)
    axes[0].set_ylabel(ylabel, fontsize=8, y=0.38)

    return axes

# draw line graph for <bench-dist>
#
# each <bench-dist> may have multiple workloads.
# therefore, we collect data for all workloads belonging to that <bench-dist> and plot them together.


def draw(bench, dist, targets, workloads):
    plt.clf()
    bench_info = objs['hash']['bench_kinds'][bench]
    bd_datas = []

    # workload: insert, pos_search, ...
    for wl, wl_info in bench_info['workloads'].items():
        if wl not in workloads:
            continue

        # target: CCEH, Level, ...
        wl_datas = []
        for t, t_plot in targets.items():

            filepath = get_filepath(bench, dist, wl, t)
            if not os.path.isfile(filepath):
                continue

            threads = []
            data = []
            if bench == "throughput":
                threads, data = read_throughputs(filepath)
            elif bench == "latency":
                threads = [32]
                data = read_latency(filepath)
                data = (np.log(data) / np.log(10**3))
            else:
                print("invalid bench: {}", bench)
                exit()
            x = bench_info['x']

            wl_datas.append({'x': x, 'y': data[:len(x)], 'stddev': [
                0, 0, 0, 0, 0, 0], 'label': t_plot['label'], 'marker': t_plot['marker'], 'color': t_plot['color'], 'style': t_plot['style'], 'xlabel': wl_info['label']})

        # collect data for all workloads belonging to that <bench-dist>.
        bd_datas.append(wl_datas)

    return draw_axes(bench, bench_info['y_label'], bd_datas)


# 1. multi-threads thourghput, latency (line graph)
for obj, obj_info in objs.items():
    targets = obj_info['targets']
    bench_kinds = obj_info['bench_kinds']

    for bench, bench_info in bench_kinds.items():
        if bench == 'latency':
            tnum = 32
        else:
            tnum = ''

        for dist in bench_info['distributions']:
            plt.clf()
            if dist == 'selfsimilar':
                plot_id = "hash-{}-multi{}-{}".format(
                    bench, tnum, "self-similar")
            else:
                plot_id = "hash-{}-multi{}-{}".format(bench, tnum, dist)

            # (a), (b), (c), (d)
            workloads = ["insert", "pos_search", "neg_search", "delete"]
            axes = draw(bench, dist, targets, workloads)
            figpath = "./out/{}_abcd.png".format(plot_id)
            plt.savefig(figpath, bbox_inches='tight', pad_inches=0.02, dpi=300)
            print(figpath)
            figpath = "./out/{}_abcd.svg".format(plot_id)
            plt.savefig(figpath, bbox_inches='tight', pad_inches=0.02, dpi=300)
            print(figpath)

            # (e), (f), (g)
            workloads = ["write_heavy", "balanced", "read_heavy"]
            axes = draw(bench, dist, targets, workloads)
            figpath = "./out/{}_efg.svg".format(plot_id)
            plt.savefig(figpath, bbox_inches='tight', pad_inches=0.02, dpi=300)
            print(figpath)
            figpath = "./out/{}_efg.png".format(plot_id)
            plt.savefig(figpath, bbox_inches='tight', pad_inches=0.02, dpi=300)
            print(figpath)

    axLine, axLabel = axes[0].get_legend_handles_labels()
    draw_legend(axLine, axLabel, "./out/{}-legend.svg".format(obj))
    axLine, axLabel = axes[0].get_legend_handles_labels()
    draw_legend(axLine, axLabel, "./out/{}-legend.png".format(obj))

# # 2. single-thread throughput (bar graph)
# for obj, obj_info in objs.items():
#     targets = obj_info['targets']
#     dfs = []
#     dfs_xlabel = []

#     for ix, dist in enumerate(["Uniform", "Self Similar"]):
#         plt.clf()
#         bd_datas = []

#         for wl in "insert", "pos_search", "neg_search", "delete":

#             wl_datas = {"workload": obj_info['bench_kinds']
#                         ['throughput']['workloads'][wl]['label_single']}

#             for t, t_plot in targets.items():
#                 filepath = get_filepath('throughput', dist.replace(" ", ""), wl, t)

#                 # filepath = "./out/THROUGHPUT/{}/{}/{}.out".format(
#                 #     dist.upper(), wl, t)

#                 if not os.path.isfile(filepath):
#                     continue

#                 _, data = read_throughputs(filepath)
#                 wl_datas[t] = data[0]
#             bd_datas.append(wl_datas)

#         dfs.append(pd.DataFrame.from_dict(bd_datas))
#         dfs_xlabel.append('('+chr(ix+ord('a'))+') '+dist)

#     # draw graph, not save
#     fig, axes = plt.subplots(1, 2, figsize=(6, 1.0))
#     for ix, df in enumerate(dfs):
#         colors = [objs["hash"]["targets"][target]["color"]
#                   for target in df.columns[1:]]
#         p = df.plot(ax=axes[ix], x="workload",
#                     xlabel=dfs_xlabel[ix], kind="bar", rot=0, legend=False, color=colors)

#         axes[0].set_yticks([0, 0.5, 1, 1.5])
#         axes[1].set_yticks([0, 1, 2])
#         axes[ix].tick_params(axis='y', labelsize=7)
#         p.tick_params(axis='x', labelsize=5)
#         p.set_xlabel(dfs_xlabel[ix], fontsize=7)
#         p.grid(True, axis='y', linestyle='--')
#     axes[0].set_ylabel("Throughput (M op/s)", fontsize=7)

#     # save
#     figpath = "./out/hash-throughput-single.svg"
#     plt.savefig(figpath, bbox_inches='tight', pad_inches=0, dpi=300)
#     print(figpath)

