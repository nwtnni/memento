from os.path import exists
import pandas as pd
import matplotlib.pyplot as plt
import numpy as np
import git
import traceback
import warnings
warnings.simplefilter(action='ignore', category=FutureWarning)

objs = {
    "queue": {
        "targets": {
            "memento_queue_general": {'data_id': '', 'label': "MSQ-mmt-O0", 'marker': 'o', 'color': 'k', 'style': '-'},
            "memento_queue_lp": {'data_id': '', 'label': "MSQ-mmt-O1", 'marker': 'd', 'color': 'k', 'style': '-'},
            "memento_queue": {'data_id': '', 'label': "MSQ-mmt-O2", 'marker': 'x', 'color': 'k', 'style': '-'},
            "memento_queue_comb": {'data_id': '', 'label': "CombQ-mmt", 'marker': 'v', 'color': 'k', 'style': '-'},
            'durable_queue': {'data_id': '', 'label': "DurableQ", 'marker': 's', 'color': 'hotpink', 'style': '--'},
            'log_queue': {'data_id': '', 'label': "LogQ", 'marker': 's', 'color': 'c', 'style': '--'},
            'dss_queue': {'data_id': '', 'label': "DssQ", 'marker': 's', 'color': 'orange', 'style': '--'},
            'pbcomb_queue': {'data_id': '', 'label': "PBCombQ", 'marker': 's', 'color': 'red', 'style': '--'},
            # 'pbcomb_queue_full_detectable': {'data_id': '', 'label': "PBCombQ+D", 'marker': 's', 'color': 'darkred', 'style': '--'},
            'clobber_queue': {'data_id': '', 'label': "ClobberQ", 'marker': 's', 'color': 'm', 'style': '-.'},
            'pmdk_queue': {'data_id': '', 'label': "PMDKQ", 'marker': 's', 'color': 'skyblue', 'style': '--'},
            'crndm_queue': {'data_id': '', 'label': "CorundumQ", 'marker': 's', 'color': 'green', 'style': '--'},
        },
    },

    # TODO: other obj
}


def draw_legend(line, label, figpath):
    plt.clf()
    legendFig = plt.figure("Legend plot")
    legendFig.legend(line, label, loc='center',
                     ncol=len(line))
    legendFig.savefig(figpath, bbox_inches='tight')
    print(figpath)


def draw(xlabel, ylabel, datas, output, x_interval=4):
    plt.clf()
    plt.figure(figsize=(4, 3))
    markers_on = (datas[0]['x'] == 1) | (datas[0]['x'] % x_interval == 0)

    for data in datas:
        # plt.errorbar(data['x'], data['y'], data['stddev'], label=data['label'], color=data['color'],
        #              linestyle=data['style'], marker=data['marker'], markevery=markers_on)
        plt.plot(data['x'], data['y'], label=data['label'], color=data['color'],
                 linestyle=data['style'], marker=data['marker'], markevery=markers_on)
    ax = plt.subplot()
    ax.xaxis.set_major_locator(plt.MultipleLocator(x_interval))
    plt.grid(True)
    plt.xlabel(xlabel, size='large')
    if ylabel != '':
        plt.ylabel(ylabel, size='large')

    # Crop the top of figure
    if output == './out/queue-throughput-pair':
        plt.ylim([-0.1, 1.6])
    elif output == './out/queue-throughput-prob20':
        plt.ylim([-0.1, 2.5])
    elif output == './out/queue-throughput-prob50':
        plt.ylim([-0.1, 3.5])
    elif output == './out/queue-throughput-prob80':
        plt.ylim([-0.1, 2.8])

    # Make red area
    x_range, y_range = plt.xlim(), plt.ylim()
    plt.fill_between([49, x_range[1]], y_range[0], y_range[1], alpha=0.08, color='red')
    plt.xlim(x_range)
    plt.ylim(y_range)

    # Save
    plt.tight_layout()
    figpath = "{}.png".format(output)
    plt.savefig(figpath, bbox_inches='tight', pad_inches=0.02, dpi=300)
    print(figpath)
    figpath = "{}.svg".format(output)
    plt.savefig(figpath, bbox_inches='tight', pad_inches=0.02, dpi=300)
    print(figpath)

    return ax


for obj in objs:
    targets = objs[obj]['targets']

    # preprocess data
    data = pd.DataFrame()
    for t in targets:

        data_id = objs[obj]['targets'][t]['data_id']

        repo = git.Repo(search_parent_directories=True)
        data_path = ''
        for commit in repo.iter_commits():
            data_path = "./out/{}_{}.csv".format(t, commit.hexsha[:7])
            if exists(data_path):
                break
        if data_id != '':
            data_path = "./out/{}_{}.csv".format(t, data_id)
        if data_path == '':
            data_path = "./out/{}.csv".format(t)

        print("read {} for target {}".format(data_path, t))
        data = data.append(pd.read_csv(data_path))

    # get stddev
    stddev = data.groupby(['target', 'bench kind', 'threads'])['throughput'].std(
        ddof=0).div(pow(10, 6)).reset_index(name='stddev')
    stddev = stddev.groupby(['target', 'bench kind'])[
        'stddev'].apply(list).reset_index(name="stddev")

    # get throughput
    data = data.groupby(['target', 'bench kind', 'threads'])[
        'throughput'].mean().div(pow(10, 6)).reset_index(name='throughput')
    threads = np.array(list(set(data['threads'])))
    data = data.groupby(['target', 'bench kind'])['throughput'].apply(
        list).reset_index(name="throughput")

    # draw graph per (obj, bench kind) pairs. (e.g. queue-pair, queue-prob50, ..)
    kinds = set(data['bench kind'])
    for ix, k in enumerate(kinds):
        plot_id = "{}-throughput-{}".format(obj, k)
        plot_lines = []

        # Gathering info
        for t in targets:
            label = targets[t]['label']
            shape = targets[t]['marker']
            color = targets[t]['color']
            style = targets[t]['style']
            marker = targets[t]['marker']
            throughputs = data[(data['target'] == t) &
                               (data['bench kind'] == k)]
            stddev_t = stddev[(stddev['target'] == t) &
                              (stddev['bench kind'] == k)]

            if throughputs.empty:
                continue
            throughputs = list(throughputs['throughput'])[0]
            stddev_t = list(stddev_t['stddev'])[0]

            if len(threads) > len(throughputs):
                gap = len(threads)-len(throughputs)
                throughputs += [None]*gap
                stddev_t += [0]*gap
            plot_lines.append({'x': threads, 'y': throughputs,
                              'stddev': stddev_t, 'label': label, 'marker': shape, 'color': color, 'style': style})

        # Draw
        if k == 'pair':
            ylabel = 'Throughput (M op/s)'
        else:
            ylabel = ''
        ax = draw('Threads', ylabel,
                  plot_lines, "./out/{}".format(plot_id), 8)
    axLine, axLabel = ax.get_legend_handles_labels()
    print(axLabel)
    draw_legend(axLine, axLabel, "./out/{}-legend.png".format(obj))
    draw_legend(axLine, axLabel, "./out/{}-legend.svg".format(obj))
